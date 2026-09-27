# KPIs, change marks, forecasts, reports and push (ADR-0051, ADR-0070)

The KPIs are derived from the queue's events by fixed rules, never by a model: nothing is stored but the events, so `kpi` and `report` can always recompute them. All the commands below print JSON. The full rules are in `docs/design/supervisor-lifecycle/kpi.md`, `marks.md`, `stats.md` (完了見込み), `report.md` and `push.md` of the dagq repository.

## `dagq kpi`: periods, kinds, targets

```sh
"$DAGQ" kpi                                  # the last 7 days, the latest (partial) last
"$DAGQ" kpi --period week --last 4           # ISO weeks
"$DAGQ" kpi --kind runtime --by parallel     # only the runtime tasks' strata, also split by parallel
"$DAGQ" kpi --since CURSOR --until CURSOR    # one window, next to the window of the same length before it
```

- **Periods.** `--period day|week` (a day from midnight in the host's local timezone; an ISO week from Monday), `--last N` (default 7), `--at <cursor>` (the latest period is the one holding it). `--since` / `--until` give one `window` instead. A cursor is an event ID, `@<unix seconds>` or an RFC 3339 time. `--goal ID` counts only that goal's runs, asks and findings.
- **Each period** has `label`, `start` / `end`, `partial` (not over yet: values so far, never judged), `runs`, `kpis`, `details`, `unavailable` (a KPI with no record and why, e.g. `candidates` `no_samples`), `marks` (the change marks in it) and `comparison`.
- **`kpis`** is KPI → stratum → value. Strata: `all`, `kind=<label>` (the task's `--kind`; `unknown` without one), and with `--by`: `build=`, `parallel=`, `slot=`, `load=`, `toolchain=` (only when the queue's repository is the dagq source), `claude=` (the claim's attributes; `unknown` when not recorded). The `plan.*` KPIs always have their own strata (`model=`, `effort=`, `origin=`, `follow_up_depth=`, `related=`, `revise_count=`), whatever `--by` says. A value has `n` (samples) and `value` (rates, counts) or `median` / `p90` / `min` / `max` (durations in seconds). `null` means no record, not zero. Compare runtime work only within `kind=runtime`: kinds differ by an order of magnitude, so `all` moves with the mix of tasks.
- **Main KPIs.** Flow: `landings`, `lead_time` (first `ready` to landing), `phase.startup|work|validate|wait_to_land`, `land_phase.*`, `slot_usage`. Rework: `first_pass_rate`, `revise_rate`, `conflict_rate`, `verification_failed_rate`, `resumes_per_run`, `failed_rate`. People: `asks_per_landing`, `attentions_per_landing`, `ask_wait`, `ask_apply_wait`. Infrastructure: `backend_failures_per_run`, `max_load_avg`, `auto_repairs`, `verify_command.<command>`. Improvements: `findings_open`, `finding_resolve_time`, `drafts_per_landing`, `draft_backlog`. Sessions: `session_open|active.<kind>`. Plan quality: `plan.*`.
- **`comparison`**: per KPI and stratum, `previous`, `delta`, `ratio`, a day's `baseline_7d` (the median of the 7 days before), `judged` with `reason` (`partial`, `small_sample` below `[kpi] min_samples`, `no_value`) and `verdict` (`improved` / `worsened` / `unchanged`). Do not read a trend from an unjudged period.
- **`targets`**: per target, `state` (`ok`, `missed`: off target but not yet long enough, `breach`: off target `breach_periods` days (default 3) or `breach_weeks` weeks (default 2) in a row, `not_judged`), `streak`, `breach_since`, `source` (`repository` / `host`) and the judged `values`. Targets are `[kpi.targets."<KPI>"]` (`kind`, `stat`, `min` / `max`; another kind's target in `[kpi.targets."<KPI>".<label>]`) in the main checkout's `dagq.toml` and in `host.toml` (below); host wins, except `[kpi] max_improvement_proposals` (dagq.toml only). There is no default target. Old binaries reject a `[kpi]` table in `dagq.toml`, so add it only once the fixed binary knows it.

## Before and after a change: `mark`, `marks`, `kpi --compare`

- **Record a change** the KPIs cannot see by themselves (a setting, the operation, the host): `"$DAGQ" mark '<label>' --note '<what and why>'`, with `--at <cursor>` when it took effect earlier (never a future time). A mistaken one: `"$DAGQ" mark --retract <id>` (only `dagq mark` and `[run.env]` marks, once). Inbox, planners and people may mark; the observer and jobs may not.
- **Marks recorded by the runtime**, never by hand: `supervisor_started` (with `dagq_version`, `parallel`, `mode`, `handoff`), `supervisor_stopped`, `run_env_changed` (the changed `[run.env]` key names; values never). **Derived** from the claims, nothing written: `derived:dagq_version`, `derived:claude_version`, `derived:parallel`, `derived:toolchain`. So a new build, a new `--parallel`, a Claude Code update or a `[run.env]` edit needs no `mark`.
- `"$DAGQ" marks [--since C] [--until C]` lists them by when they took effect: `id` (null for a derived one), `kind`, `at`, `label`, `retracted_by`, `detail` (a derived mark's `claim_event` is its ID for `--compare`).
- `"$DAGQ" kpi --compare <mark id | claim event id | cursor> [--window 7]` compares the `--window` days before and after; `--compare A..B,C..D` two explicit windows. Read `split` (`separable: false`: marks too close together to tell apart), `confounders` (other marks before, between and after the windows: name them when you report the result), `strata` (`before` / `after` per KPI and stratum, with the same verdicts) and `summary` (per kind, `lead_time`, `phase.*`, `land_phase.*`; pick the kind with `--kind`).

## `dagq forecast`: when the open work will finish

```sh
"$DAGQ" forecast                     # every open task and goal
"$DAGQ" forecast --goal ID           # that goal and its tasks (--task ID: that task and its goal)
"$DAGQ" forecast --parallel 2        # as if the slots were 2; --trials N (default 1000)
```

It simulates the queue (method 1): `ready` / `in_progress` tasks outside draft goals, in claim order (as `graph`), on the live supervisors' `parallel` summed, each drawing work, validate and wait to land together from one landed run of its kind (fewer than `[kpi] min_samples` runs: the whole distribution). A run in flight draws only the rest of its phase; a run waiting for a person holds no slot and adds an `ask_wait`. A goal closes when its open tasks are done plus a close delay. The same queue at the same time gives the same numbers (the seed is the time and the latest event ID). It reads only and records nothing; the observer, planners and plan review may run it.

- **Output**: `method`, `at`, `seed`, `trials`; `assumptions` (`parallel`, `min_samples`, `samples` per kind and for `close_delay` / `ask_wait`, `substituted`: kinds that used `all`, `left_out`); `tasks[]` (`id`, `goal_id`, `kind`, `phase`, `waiting`, `distribution`) and `goals[]` (`id`, `open_tasks`, `unplanned_tasks`), each with `p50` / `p90` (times) and `p50_secs` / `p90_secs` (seconds from now).
- **It is "if the current plan flows as it is"**: no inflow (new tasks, follow_up drafts, plan review sending work back) and no retries of failed runs. Real finishes run later than this, most for goals with many follow_ups. `draft` / `submitted` tasks and draft goals are out; what waits on them, and a goal with unplanned (`unplanned_tasks` > 0) or no tasks, has `p50` / `p90` null with `reason: blocked`. Other reasons: `no_samples` (no landed run yet), `no_slots` (`parallel` 0).
- **Report it** with p50 and p90 together and its premises: `parallel`, `substituted` kinds, `unplanned_tasks`, and that inflow is not included. Never give p50 alone as a promise.

## Did the forecasts come true: `forecast.*` KPIs

The supervisor records a snapshot of the unfiltered forecast (`forecast_recorded`) after a plan review pass (or a concern answered `ready`), a change mark or a priority / dependency change of a forecast task, a landing that moved some p50 by 20% of its remaining time and 30 minutes or more (or between a time and null, or a new target), and once a day; `dagq forecast` itself never records one. `kpi` scores each snapshot row against its target's finish (a task `completed`, a goal closed `achieved`), in the period of the finish:

- `forecast.p50_error`: actual − p50 in seconds (positive: late; the median's sign is the bias), `forecast.p50_abs_error` (lower is better), `forecast.p50_error_ratio` (error ÷ the forecast's remaining time), `forecast.p90_hit_rate` (finished by p90; about 90% is right), `forecast.late_rate` / `forecast.early_rate`.
- Strata, whatever `--by` says: `all`, `target=task|goal`, `kind=` (tasks), `band=` (the remaining time p50 gave: `0-1h`, `1-6h`, `6-24h`, `1-3d`, `3d+`), `method=`, `marks=0` / `marks=1+` (change marks and priority or dependency changes between the snapshot and the finish). `marks=0` is the method's own error; its gap to `marks=1+` is the drift from plan changes.
- `details.forecast`: `samples`, `with_marks`, `marks_between` (`median` / `max`), `excluded` (`canceled` tasks, `abandoned` goals, `unforecast` rows with no p50: counted, never scored). `--goal ID` keeps that goal and its tasks.
- No target by default; a planner may add `[kpi.targets."forecast.p50_error_ratio"]` (e.g. `min = -0.25`, `max = 0.25`) and `[kpi.targets."forecast.p90_hit_rate"]` (e.g. `min = 0.75`) to `dagq.toml`, judged like any other target.

## Reports

The supervisor (`supervise --report-daily`, on by default) writes, once per local day, the missing reports of the 7 days before today and of the last ISO week under `<queue dir>/reports/`: `daily/YYYY-MM-DD.{html,json}`, `weekly/YYYY-Www.{html,json}` and `index.html` (newest first). The HTML is one self-contained page (targets, trends with the marks, the forecast errors, the KPI table, the top open findings); the JSON is `kpi --period P --at <that period>` plus `report` and `findings`. Retention is `[report] keep_daily_days` (90) and `keep_weekly_weeks` (104) of `host.toml`. Each written report records `report_written`.

`"$DAGQ" report [--period day|week] [--at C] [--out DIR]` writes one by hand the same way (today's and this week's are `.partial`), and prints the paths; `--print json` writes nothing. It records nothing and changes no queue state. Find the queue directory from `db` in `"$DAGQ" --resolve`.

The **forecast error** section scores the targets that finished in the report's period: sample, with-marks and excluded counts, then per stratum (`all`, `target=`, `kind=`, `band=`, `marks=`, `method=`) `n`, the signed median error (+ late), the median |error|, the median ratio, p90 hit, late / early, the bias (the larger of late and early, or `even`) and the worst state of any `forecast.*` target. No samples, no table. Read a bias only with enough `n`, and `marks=0` before `marks=1+`. Push content has no forecast.

## Push to a person away from the screen

`host.toml`, never the committed `dagq.toml`, holds the push command: `<queue dir>/host.toml` (this queue) or `$XDG_CONFIG_HOME/dagq/host.toml` (default `~/.config/dagq/host.toml`, every queue of the host); the queue's `[push]` replaces the host-wide one as a whole, and `command = []` turns it off for that queue.

```toml
[push]
command = ["/Users/me/.local/bin/dagq-push-ntfy"]   # argv, no shell; required
timeout_secs = 30
daily = true               # the daily and weekly summaries
breach = true              # a message when a target breach starts
max_breach_per_day = 3
```

After writing the reports, the supervisor records `kpi_breach_started` / `kpi_breach_resolved` when a target's state changes (with or without `[push]`), then passes each message to the command on stdin as one JSON object (`kind`, `title`, `text`, `breaches`, `missed`, `resolved`, `open_asks`, `report_html` / `report_json`), with `DAGQ_PUSH_KIND` (`daily` / `weekly` / `breach`), `DAGQ_QUEUE`, `DAGQ_REPORT_HTML`, `DAGQ_REPORT_JSON`. A failed message is tried again after 1 and 5 minutes; after three failures the inbox gets `fix the push command` (`kpi_push_abandoned`), cleared by the next success. Keep secrets (webhook URLs, topics, tokens) in the script, a file or the supervisor's environment, outside the repository; events never record the argv or the message. Script examples for ntfy and Slack: `push.md` of the design docs.
