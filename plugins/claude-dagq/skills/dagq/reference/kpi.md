# KPIs, change marks, forecasts, reports and push

Each command below prints JSON; its flags, strata and output are in `"$DAGQ" <command> --help`. This file says how to read them and what to do.

## `dagq kpi`: periods, strata, targets

```sh
"$DAGQ" kpi --period week --last 4           # ISO weeks, the latest (partial) last
"$DAGQ" kpi --area <area> --by provider      # only the runs that changed that area, also split by provider
"$DAGQ" kpi --since CURSOR --until CURSOR    # one window, beside the window of the same length before it
```

- `null` is no record, not zero. A `partial` period (not over yet) and a value with fewer samples than `[kpi] min_samples` are not judged: read no trend from them.
- `change=` is the change the task declared, never guessed from the diff. `area=` comes from the landed diff through `dagq.toml`'s `[areas]`, so editing `[areas]` reclassifies past runs, and a run that touched several areas counts in each.
- Compare work time only within one `area=` or `change=` stratum (which one is the repository's rule): tasks differ by an order of magnitude, so `all` moves with the mix of tasks.
- A headless job's provider is `job.*`'s own `provider=` stratum, not the worker's `--by provider`.
- In `plan.*`, `model=` / `effort=` are the plan review job's session and `planner_model=` / `planner_effort=` the span of the planner whose submission was the last before that review (before the proposal became ready, when no review judged it), so a proposal several planners worked on counts under the last one; `unknown` when that span or its record is missing.
- **Tokens**: a `null` value means the records do not cover the period (the output says why); never read it as 0.
- **Targets** are `[kpi.targets."<KPI>"]` in the main checkout's `dagq.toml` and in `host.toml` (host wins, but `[kpi] max_improvement_proposals` is `dagq.toml`'s only); only the targets set there are judged. `breach` is off target for several periods in a row, `missed` not yet that long. Add a `[kpi]` table to `dagq.toml` only once the installed binary knows it: an older one rejects it.

## Before and after a change: `mark`, `marks`, `kpi --compare`

- Mark a change the KPIs cannot see by themselves (a setting, the operation, the host) when it takes effect: `"$DAGQ" mark '<label>' --note '<what and why>'` (`--at <cursor>` when it took effect earlier); take a mistaken one back with `mark --retract <id>`. Inbox, planners and people may mark; the observer and jobs may not.
- Never mark what the runtime marks or derives itself: a new build, a supervisor's start or stop, a new `--parallel`, a Claude Code or Codex update, a `[run.env]` edit. `"$DAGQ" marks` lists both kinds.
- `"$DAGQ" kpi --compare <mark | claim event id | cursor>` compares the `--window` days before and after. Drop the result when `split.separable` is false, name its `confounders` when you report it, and read it within one stratum as above. A mark that only records a build is a confounder, never the change: read builds in the `build=` strata.

## Today's values beside the ledger: `stats --ledger`

`"$DAGQ" stats --ledger` prints today's values beside the ledger's, each difference with its reason. Report an `unexplained` one; correct neither side by hand.

## Raising throughput: the weekly review

Any queue, once a week, in this order (`kpi --period week --last 4` for the numbers, `kpi --period day` for the days). Keep the time runs wait for a person (`ask_wait`, and `stats`' `waiting`) apart from their own time and show both.

1. **Check the arithmetic.** landings per hour ≈ slots in use ÷ slot time per landing (slots in use = `slot_usage` × `parallel`; slot time per landing ≈ the mean of `work` + `validate` + `wait_to_land`, not `startup`, which lies inside `work`, minus the time runs left their slot to wait for a person). If the two sides are far apart, doubt the records or your reading before anything else.
2. **Find the one constraint.** Look for what sits at 1: `slot_usage` (every slot busy), `load_per_core` (`p90` at 1 or above: the CPU is saturated; `max_load_avg` is only a peak of sampled moments), `landing_utilization` (the serial landing slot: near 1, or `landing_utilization.peak` at 1 with `landing_queue_depth` above 0, means landings are capped by one verify at a time), `ask_wait` (runs held by people). Only the constraint is worth fixing; a faster non-constraint lands nothing more. Slots full and CPU saturated: do not add slots, lower `cpu_per_landing` (its per-process split says where the CPU goes).
3. **Pareto of slot time.** Sort `phase.work|validate|wait_to_land` by median × count, largest first, then split the top one (`land_phase.*` breaks down `wait_to_land`; `phase.startup` is part of `work`), never adding a part to its whole; fix only the top one or two. With the landing slot as the constraint, `land_phase.*` and `landing_attempt` are the list.
4. **Outliers.** Read `p90` and `max`, not only the median. Take the longest runs in `stats --full` (by `work` + `validate` + `wait_to_land`, which still hold the time a run waited for a person) and read each with `timeline RUN` (the gaps and their reasons, a person's wait among them); a few long runs often cost more than every median together. `first_pass_rate` and `verification_failed_rate` tell how much slot time is rework; `review.sendback_rate` and `plan.revise_rate` split the send-backs by their primary code (`code=`), and `details.review_reasons` has each code's time and answers.
5. **One change, one mark, a fair comparison.** Make a single change, `mark` it when it takes effect (not needed for a build, `--parallel`, Claude Code or `[run.env]`), wait `--window` days, then `kpi --compare <mark>` within one stratum: `--area <name>` or `--change <change>` (which one is the repository's rule), never `all`, which moves with the mix of tasks. Name the `confounders`, drop the result when `split.separable` is false, and do not read a verdict below `[kpi] min_samples` or from a `partial` period.
6. **Cadence.** Hourly: only whether it has stopped (landings per hour on a 3–6 hour moving average; do not react to one hour's rise or fall). A quiet hour's review stays short: what is steady, and any sign that would matter if it went on. Daily: the outliers (4) and the target `breach`es. Weekly: 1 to 5.

## `dagq forecast`: when the open work will finish

```sh
"$DAGQ" forecast                     # every open task and goal
"$DAGQ" forecast --goal ID           # that goal and its tasks (--task ID: that task and its goal)
"$DAGQ" forecast --parallel 2        # as if the slots were 2
```

It reads only; the observer, planners and plan review may run it. It is "if the current plan flows as it is": no new tasks, follow_up drafts, send-backs or retries, so real finishes come later, most for goals with many follow_ups.

Report it with p50 and p90 together and its premises: `parallel`, the changes that borrowed the whole distribution (`substituted`), `unplanned_tasks`, and that inflow is not included. Never give p50 alone as a promise.

## Did the forecasts come true: `forecast.*` KPIs

`kpi` scores the supervisor's forecast snapshots against the finishes. Read a bias (`forecast.p50_error`, positive when late; `forecast.p90_hit_rate`, about 90% is right) only with enough `n`, and `marks=0` (the method's own error) before `marks=1+` (plan changes between). A planner may add `[kpi.targets."forecast.<metric>"]` targets.

## Reports

The supervisor writes the daily and weekly reports under `<queue dir>/reports/` (the directory of `db` in `"$DAGQ" --resolve`); a person opens `index.html`. `"$DAGQ" report` writes one by hand and changes no queue state.

## Push to a person away from the screen

The push command lives in `host.toml`, never the committed `dagq.toml`: `<queue dir>/host.toml` for this queue, or the host-wide `dagq/host.toml` of the user's config directory for every queue of the host; the queue's `[push]` replaces the host-wide one whole, and `command = []` turns it off.

```toml
[push]
command = ["/Users/me/.local/bin/dagq-push-ntfy"]   # argv, no shell; required
```

The command reads each message as one JSON object with `title` and `text` on stdin. It also runs for the inbox's watch messages, in the inbox session's environment, not the supervisor's. Keep secrets (webhook URLs, topics, tokens) in the script or a file outside the repository. A message that keeps failing reaches the inbox as `fix the push command`, cleared by the next success. A script for ntfy can pipe `jq -r .text` to `curl -fsS -H "Title: <.title>" --data-binary @- https://ntfy.sh/<topic>`.
