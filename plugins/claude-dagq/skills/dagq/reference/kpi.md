# KPIs, change marks, forecasts, reports and push

Each command below prints JSON; `"$DAGQ" <command> --help` lists its flags (and the strata `kpi` can split by), and the values are in the JSON output. This file says how to read them and what to do.

`stats`, `kpi` and `report` read the queue's ledger, which the supervisor builds on its passes: what no record holds reads as missing (with why), not as 0. `stats` gives each listed run's time and slot time by blocker, holds and attempt in `breakdown` (overall, `by_change`, `by_area`), each axis adding up to 100% with `unattributed`; read the unattributed share there before trusting the rest. The time runs from the claim to the end of the push. `stats --ledger` is still stopped and refuses with why.

## `dagq kpi`: periods, strata, targets

```sh
"$DAGQ" kpi --period week --last 4           # ISO weeks, the latest (partial) last
"$DAGQ" kpi --area <area> --by provider      # only the runs that changed that area, also split by provider
"$DAGQ" kpi --since CURSOR --until CURSOR    # one window, beside the window of the same length before it
```

- A missing value is no record, not zero. A period not over yet and a value with too few samples are not judged: read no trend from them.
- Compare work time only within one `area=` or `change=` stratum (which one is the repository's rule): tasks differ by an order of magnitude, so `all` moves with the mix of tasks.
- **Time by tag.** Each period's `breakdown` holds its finished runs' time and slot time by blocker, holds and attempt, in `all` and per `change=` and `area=` stratum (`--change` and `--area` keep only theirs), each axis adding up to 100% with `unattributed`; the daily report shows it as "Time by tag". `unattributed_ratio` is the share of the time no record accounts for, a KPI of every stratum (judged only against a target set for it): when it is high, the split is not to be trusted, and the fix is a missing record, not the work.
- `--by` (and so `--cross` and `--compare`) takes any key of the claims' attributes the ledger's run rows hold, also one no claim records (every run reads `unknown`). The derived layers (`provider`, `claude`, `model`, `group`, `slot`, `load`, `toolchain`, `change`, `area`, `nature`) are made by their own rules from the attributes, the facts after the claim and the run (the provider it ended on; a run that worked on Codex is `claude=none`, `group=none` and of its Codex turn's model), never from an attribute of the same name; any other key is the attribute as the claim recorded it.
- **Provider pair trial.** `--by pair_group` compares each assigned group in overlapping strata with the same items: the primary (runs that ran as assigned and whose failure a recovery job did not judge the environment's), the runs that switched, the runs the environment caused the failure of, and the secondary (every run of the group). Compare quality in the primary, and read the group's switched runs and the runs that cannot be judged in the secondary. A run with no judgement stays where it was: it does not say the environment was not the cause. The strata's counts need not add up to the secondary's.
- **Targets** are set in the repository's `dagq.toml` and the host's `host.toml`; only the targets set there are judged. Act on a target only when the output says it is breached (off target for several periods in a row), not on a single miss. Add a `[kpi]` table to `dagq.toml` only once the installed binary knows it: an older one rejects it.

## Before and after a change: `mark`, `marks`, `kpi --compare`

- Mark a change the KPIs cannot see by themselves (a setting, the operation, the host) when it takes effect: `"$DAGQ" mark '<label>' --note '<what and why>'` (`--at <cursor>` when it took effect earlier); take a mistaken one back with `mark --retract <id>`. Inbox, planners and people may mark; the observer and jobs may not.
- Never mark what the runtime marks itself (`"$DAGQ" marks` lists both kinds).
- **Instruction versions.** Each worker run's claim records a hash of the content of what its worker read: the runtime's worker prompt before the task's values (`instructions_prompt`), the plugin its session loaded (`instructions_plugin`, `none` without one) and the repository's instruction documents at the run's base (`instructions_repo`). Runs that read the same text share the value, so a change of the instructions needs no mark: `kpi --by instructions_repo` gives each version its n, median and p90, and `kpi --compare <cursor> --by instructions_repo` reads the version before beside the one after. Split where the version changes: the repository's documents at the landing of their change (runs take their base after it), the prompt and the plugin when the new binary or plugin is installed, not when its change lands; when unsure, at the claim of the first run carrying the new value. `unknown` is a run claimed before versions were recorded or one whose hash could not be found: leave it out of the comparison. Read the versions within one area or change: `--area` and `--change` only choose which of their own strata are listed, so `instructions_repo=<hash>` alone mixes the runs of every area. Add `--cross` (`kpi --compare <cursor> --area <name> --by instructions_repo --cross`) and read the `cross:area=<name>|instructions_repo=<hash>` strata. Name the other versions that changed in the same window as confounders.
- `"$DAGQ" kpi --compare <mark | claim event id | cursor>` compares the `--window` days before and after. Drop the result when the output says the change cannot be separated from others, name the confounders it lists when you report it, and read it within one stratum as above. A mark that only records a build is a confounder, never the change: read builds in the `build=` strata.

## Today's values beside the ledger: `stats --ledger`

`"$DAGQ" stats --ledger` prints today's values beside the ledger's, each difference with its reason. Report a difference with no explanation; correct neither side by hand.

## Raising throughput: the weekly review

Any queue, once a week, in this order (`kpi --period week --last 4` for the numbers, `kpi --period day` for the days). Keep the time runs wait for a person apart from their own time and show both.

1. **Check the arithmetic.** Compare the week's slot occupancy measured from the slots with the one expected from the landings and their slot time (both are in the week's JSON output). They come from different records: when they agree, the slot numbers can be read; when they do not, doubt the records or your reading before anything else, and name the value and the cause you found. A day strays more than a week.
2. **Find the one constraint.** Arithmetic checked, look for what sits at 1: the share of slots held, the CPU load per core, the serial landing slot's utilization, and the time runs wait for people. The share held can pass 1: a run waiting to land keeps its slot while a light change claims the room its finished work leaves and holds another, so 1 alone does not say every slot is at work; read it with the landing queue and `candidates`. Only the constraint is worth fixing; a faster non-constraint lands nothing more. Slots full and CPU saturated: do not add slots, lower the CPU each landing takes.
3. **Pareto of slot time.** Sort the phases of slot time by median × count, largest first, then split the top one into its parts, never adding a part to its whole; fix only the top one or two. With the landing slot as the constraint, the landing's own phases and attempts are the list.
4. **Outliers.** Read `p90` and `max`, not only the median. Take the longest runs in `stats --full` and read each with `timeline RUN` (the gaps and their reasons, a person's wait among them); a few long runs often cost more than all medians. The first-pass and verification-failure rates tell how much slot time is rework; the send-back rates of reviews and plan reviews, split by their primary code, tell why.
5. **One change, one mark, a fair comparison.** Make a single change, `mark` it when it takes effect (not what the runtime marks itself), wait the comparison window, then `kpi --compare <mark>` within one stratum: `--area <name>` or `--change <change>` (which one is the repository's rule), never `all`, which moves with the mix of tasks. Name the confounders the output gives, drop a result it says cannot be separated, and read no verdict from too few samples or a period not over yet.
6. **Cadence.** Hourly: only whether it has stopped (landings per hour on a 3–6 hour moving average; do not react to one hour's rise or fall). A quiet hour's review stays short: what is steady, and any sign that would matter if it went on. Daily: the outliers (4), the breached targets and the time by tag, its unattributed share first. Weekly: 1 to 5.

## `dagq forecast`: when the open work will finish

```sh
"$DAGQ" forecast                     # every open task and goal
"$DAGQ" forecast --goal ID           # that goal and its tasks (--task ID: that task and its goal)
"$DAGQ" forecast --parallel 2        # as if the slots were 2
```

It reads only. It is "if the current plan flows as it is": no new tasks, follow_up drafts, send-backs or retries, so real finishes come later, most for goals with many follow_ups.

Report it with p50 and p90 together and the premises the output names, and that inflow is not included. Never give p50 alone as a promise.

## Did the forecasts come true: `forecast.*` KPIs

`kpi` scores the forecasts against the finishes. Read a bias (a p90 hit rate of about 90% is right) only with enough samples, and the method's own error (no marks between) before plan changes. A planner may add forecast targets.

## Reports

The supervisor writes the daily and weekly reports in the queue's directory (`report --help`); a person opens them in a browser. `"$DAGQ" report` writes one by hand and changes no queue state.

## Push to a person away from the screen

The push command lives in the host's `host.toml` (the queue's own or the host-wide one), never the committed `dagq.toml`; `command = []` turns it off.

```toml
[push]
command = ["/Users/me/.local/bin/dagq-push-ntfy"]   # argv, no shell; required
```

The command reads each message as one JSON object with `title` and `text` on stdin. Keep secrets (webhook URLs, topics, tokens) in the script or a file outside the repository. A message that keeps failing reaches the inbox as `fix the push command`. A script for ntfy can pipe `jq -r .text` to `curl -fsS -H "Title: <.title>" --data-binary @- https://ntfy.sh/<topic>`.
