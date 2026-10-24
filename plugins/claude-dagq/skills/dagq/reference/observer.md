# Findings: what the observer saw, and how they are decided

The observer is a headless job the supervisor starts on a schedule. It records what goes wrong as **findings** and writes no note, goal or task and changes no run. Run it by hand with `"$DAGQ" observe`, or print its prompt with `observe --dry-run` (`observe --help`).

A finding is one record per problem; one that comes back after it was resolved means the remedy did not work.

## Read

- `"$DAGQ" findings` lists the open and proposed findings, biggest impact first; `findings ID` shows one (`findings --help`).
- `"$DAGQ" events --full --run RUN_ID` (or `--task`, `--goal`, `--kind`) reads the events around a finding; `"$DAGQ" timeline RUN_ID` shows where a run's time went (`reference/inspect.md`).
- `"$DAGQ" observe --history` lists the observations, and `observe --input OBSERVATION` reads one's whole input. Repeated failed observations reach the inbox as `check the failed observer`.

## Threshold findings

A `threshold` finding says a stall setting looks wrong; read the detections and their outcomes in `stats` (`stats --help`). Many asks answered `wait`: the threshold is probably too early. Many where a person stepped in first: probably too late. No outcome yet: do not judge on it. The observer never changes the value; changing it is the user's decision.

## Session time

For a `capacity` or `wait` finding read the sessions in `stats` (`reference/inspect.md`, "Claude session time"): a kind whose open time grows while its active share falls is waiting, not working.

## KPI findings: kind `kpi` and the improvement limit

The observer reads the KPIs (`reference/kpi.md`) with the repository's and the host's targets, and copies numbers from them, computing none.

- A breached target becomes a `kpi` finding (a forecast target's, a `forecast` finding) with the breach as evidence; a target not yet breached, or not judged, makes none, and a breach is never a `blocked` ask. The observer marks it for a proposal when the impact and the length call for a remedy.
- Read a `kpi` finding with `kpi` and the `marks` around the breach's start: a change there may be the cause, or the remedy.
- **Improvement limit.** While the open improvement work for findings reaches the repository's limit, another marked finding waits for a planner; a repair (`ci_failure`) does not wait.
- **Priority.** An improvement's tasks take their goal's priority, `normal` or below; a priority a person set stays.

## Agent watch findings: kinds `agent_eval_watch_due` and `agent_eval_below_threshold`

The supervisor records an `agent_eval_watch_due` finding for a runtime planner when an agent with case lists is due for a watch. Its detail says the step and the command to run. The planner opened for it:

1. **A watch is due**: asks for the production set the detail names (`agent cases`), then `finding resolve ID --reason 'set N asked for'`.
2. **A built set waits for its hold-out round**: first checks with `agent results` / `agent result` whether a round on the same definition already ran that set. If it did, `finding dismiss ID --reason` with why, and no rerun. Else `agent eval <agent> --set N`, then `finding resolve ID --reason 'eval E asked for'`.
3. **The round was refused** (not started): `finding dismiss ID --reason` with the refusal's reason. A planner never asks to run it again (`--rerun` is a person's); a raised limit is a person's decision.

A hold-out or production round on the landing branch's current definition that ends under the threshold is an `agent_eval_below_threshold` finding, whoever asked for it, with its scores and failed cases; a `dev` round of a definition not yet landed is not. Its planner submits a task to improve the definition on those cases (`submit ... --finding ID`), an improvement that counts and waits at the limit. A failed case whose label looks wrong is a label to decide, not the definition's: it goes to a person, as a set's disputed cases do.

## Where a finding goes

1. **A proposal, without a person.** The observer marks a finding that needs a remedy (`finding record --propose '<why>'`: it recurs or costs much). A runtime planner then checks `search` / `related` for work already queued or done and either submits tasks (`submit ... --finding ID`), dismisses it, or asks the person with a `planner_question`. When planners leave it undecided, the inbox gets `request a plan for the finding`: on the person's word it records `request add --ref finding:N`.
2. **An ask, only when its reading needs a person.** A finding whose reading is to wait or leave it stays a finding, with that reading in its detail. The observer raises `ask --kind blocked --finding ID` only when the reading needs a person's decision (scope, discard) or hands the runtime and the recovery job cannot lend, one open ask per finding (`ask --help`). The runtime applies its `propose` and `dismiss` answers itself.
3. **Decided on a person's word.** A person who wants a finding acted on asks the inbox, which records `request add --text '<their words>' --ref finding:N`; its runtime planner submits a proposal for it, dismisses it (`finding dismiss ID --reason '...'`) or leaves it to the observer's mark. `finding resolve ID --reason '...'` closes one that no longer happens. A runtime planner opened for anything may also dismiss or resolve a finding its work settles. The person may also decide in their own terminal (no `DAGQ_ROLE`).

A note (`note` / `notes`) stays the person's and planners' free-text memo; nothing waits on it. A draft goal an older observer wrote is decided like any draft on the person's word: through `request add --ref goal:N`, whose runtime planner submits it (`submit --goal ID`) or drops it (`goal close ID --verdict abandoned`).
