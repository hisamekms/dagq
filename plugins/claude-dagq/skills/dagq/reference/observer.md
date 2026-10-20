# Findings: what the observer saw, and how they are decided

The observer is a headless job the supervisor starts on a schedule. It records what goes wrong as **findings** and writes no note, goal or task and changes no run (the queue refuses that from `DAGQ_ROLE=observer`). Run it by hand with `"$DAGQ" observe`, or print its prompt with `observe --dry-run` (`observe --help`).

A finding is one record per problem (a `kind`, a target and a `subject`); recording the same again updates it, and reopens a `resolved` one (the remedy did not work).

## Read

- `"$DAGQ" findings` lists the open and proposed findings, biggest impact first; `findings ID` shows one in any status, `--full` with its evidence events (`findings --help`).
- `"$DAGQ" events --full --run RUN_ID` (or `--task`, `--goal`, `--kind`) reads the events around a finding; `"$DAGQ" timeline RUN_ID` shows where a run's time went (`reference/inspect.md`).
- `"$DAGQ" observe --history` lists the observations, and `observe --input OBSERVATION` reads one's whole input, including what its prompt left out. Repeated failed observations reach the inbox as `check the failed observer`.

## Threshold findings: reading `stall_thresholds` and `running_alerts`

A `threshold` finding says a `[stall]` setting looks wrong. Read `stats`' `stall_thresholds` (one entry per setting, with its detections and their outcomes) and `findings --kind threshold`:

- Many `answered_wait` (the ask's answer was to wait): the threshold is probably too early. Many `preempted` (a person stepped in first): probably too late. `resolved_by_*`: the detection works. `pending`: no outcome yet; do not judge on it. Compare before and after a change of the value with `by_threshold_secs`.
- An `idle_without_receipt` alert in `running_alerts` past its threshold, neither `nudged` nor `asked`, suggests a missed stall.
- The observer records it as `finding record --kind threshold --subject <setting name>`, with `--propose` when it recurs; it never changes the value. Changing it is the user's decision (`[stall]` in `dagq.toml`).

## Session time: reading `sessions`

For a `capacity` or `wait` finding read `stats`' `sessions` (`reference/inspect.md`, "Claude session time"): a kind whose open time grows while its active share falls is waiting, not working.

## KPI findings: kind `kpi` and the improvement limit

The observer reads the KPIs (`reference/kpi.md`) with the targets of `dagq.toml` and `host.toml`, and copies numbers from them, computing none.

- A target in `breach` becomes a finding `--kind kpi --queue --subject '<KPI>/<stratum>'` (e.g. `phase.work/area=<area>`) with the breach's event as evidence; the day and the week of one KPI and stratum are one finding. `missed` and `not_judged` make none, and a breach is never a `blocked` ask. The observer resolves it once the target is `ok` again, and marks it `--propose` when the impact and the length call for a remedy.
- A breach of a `forecast.*` target becomes `--kind forecast --queue --subject '<metric>/<stratum>'` without the `forecast.` prefix (e.g. `p90_hit_rate/all`), never a `kpi` finding, under the same rules. A forecast bias is judged only as such a breach.
- Read a `kpi` finding with `kpi` and the `marks` around `breach_since`: a change there may be the cause, or the remedy.
- **Improvement limit.** While the open improvement work for findings reaches `[kpi] max_improvement_proposals` of `dagq.toml`, another marked finding waits `open` for a planner (`findings` shows it as `improvements`). A CI watch's `ci_failure` finding is a repair, not an improvement: it is not counted and does not wait.
- **Priority.** An improvement's tasks take their goal's priority, `normal` or below; a priority a person set stays.

## Where a finding goes

1. **A proposal, without a person.** The observer marks a finding that needs a remedy (`finding record --propose '<why>'`: it recurs or costs much). A runtime planner then checks `search` / `related` for work already queued or done and either submits tasks (`submit ... --finding ID`), dismisses it, or asks the person with a `planner_question`. When planners leave it undecided, the inbox gets `request a plan for the finding`: on the person's word it records `request add --ref finding:N`.
2. **An ask, only when its reading needs a person.** A finding whose reading is to wait or leave it stays a finding, with that reading in its detail. The observer raises `ask --kind blocked --finding ID --because <scope|discard|recovery_failed> --recommend <option> --confidence <high|low>` only when the reading needs a person's decision (scope, discard) or hands the runtime and the recovery job cannot lend (recovery_failed), one open ask per finding. The runtime adds the options `propose` and `dismiss` and applies those answers itself.
3. **Decided on a person's word.** A person who wants a finding acted on asks the inbox, which records `request add --text '<their words>' --ref finding:N`; its runtime planner submits a proposal for it, dismisses it (`finding dismiss ID --reason '...'`) or leaves it to the observer's mark. `finding resolve ID --reason '...'` closes one that no longer happens. A runtime planner opened for anything may also dismiss or resolve a finding its work settles. The person may also decide in their own terminal (no `DAGQ_ROLE`).

A note (`note` / `notes`) stays the person's and planners' free-text memo; nothing waits on it. A draft goal an older observer wrote is decided like any draft on the person's word: through `request add --ref goal:N`, whose runtime planner submits it (`submit --goal ID`) or drops it (`goal close ID --verdict abandoned`).
