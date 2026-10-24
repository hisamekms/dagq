# Premises and Spikes: planning on what is not yet known

What the `dagq-planner` skill's "Premises and Spikes" names in short. `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"`. A **Spike** is a task whose run investigates a premise of the plan by experiment, prototype, build or measurement, instead of implementing (`--execution-class spike`). The class is independent of `--change`. How much evidence a premise needs and which checks count are the repository's rules (its AGENTS.md or CLAUDE.md and what it names), not this file's.

## 1. Find the premises, and settle the short ones yourself

Before you split the work, list the premises the plan rests on and has not confirmed: that a tool, an API, a permission, a provider's route or a headless mode behaves as the plan assumes; that a change fits the time or the size the plan gives it; that a measurement shows what the plan expects. Keep only those whose answer would change the plan (the tasks, their order, the acceptance or whether to plan at all).

Settle in your own session each one a **short read** answers: the repository's code, tests and decision records, the queue's records (`search`, `related`, `show ID --full`, `events`, receipts), and the tool's official documentation or `--help`. A short read reads and searches; it does not build, run a test suite, try a prototype or measure. Record what you read and what it settled in the task's `context` or a `note`. Never have a subagent build, experiment or measure for you: it would run outside the worker slots the runtime shares and limits. A premise you leave as an assumption is written as one (in `context`), never as a fact.

## 2. Move what needs an experiment into a Spike

Only a question that needs an experiment, a prototype, a build or a measurement becomes a Spike: one question per Spike, in the goal whose plan it decides (a further Spike always has a goal). Its description (or acceptance) holds:

- **Question**: the one premise, stated so a verdict answers it.
- **Impact**: what each answer changes in the later plan (which tasks, which approach).
- **Method**: how to investigate: what to build, try or measure, and against what.
- **Limit**: the time and the attempts to spend, written as an instruction to the agent; the receipt says what was used, so an overrun shows.
- **Evidence**: where it goes (a committed note, a script, a log path the receipt names) and what it records.
- **Per verdict**: what the plan does when the premise holds, does not hold, or stays unresolved.

The acceptance is a verdict with grounds, never "the wanted technique works": `holds`, `does_not_hold` or `unresolved`, each with its grounds and evidence. A negative or unresolved verdict is a successful investigation. The run's receipt carries it with its grounds, evidence and the conditions it was examined under.

- A premise about permissions, a headless run or a provider is checked by an experiment in the worker's own run: what a planner, a subagent or a person's terminal shows does not count as `holds`.
- No hand steps on a throwaway queue (`init`, `add`, `up` by hand) and no hand application of production settings in its acceptance: a worker cannot do them (`reference/register.md`, "Task fields"). Make the check a test, or leave it to the person or the inbox outside the task.
- `--verify`, `--paths`, `--evidence` and `--change` follow the repository's rules as for any task (`reference/scope.md`).

```sh
"$DAGQ" add "TITLE" --goal GOAL --execution-class spike --description '...' --acceptance '...' ...
"$DAGQ" edit TASK --execution-class implementation   # a draft or submitted task only
```

## 3. Submit the Spikes first, then stop

Submit the Spikes alone. The implementation they decide may wait as drafts (`add` without `submit`, its `context` naming the Spike), but never readied, and never made to start by depending on a Spike's completion alone: it is planned again from the result. Once submitted, report and stop: do not wait for the Spike's run. A new planner takes the Spike's result when it completes. A Spike you cancel records no request: decide its follow-up yourself as you cancel it.

## 4. Replanning from a Spike's result

A planner opened for a Spike's request reads the Spike's verdict, its follow_ups (planned by their own planners: plan around them) and the goal's other Spikes. First check that the evidence's conditions still hold: what changed on the default branch since the commit it examined, on the paths the question is about, and whether the tools are still those versions. Where they no longer hold, say so and weigh a further Spike instead of trusting the result. Then decide exactly one, recording why in the `context` of what you add:

1. **Go on to the implementation**: add its tasks (or edit the drafts kept for it), `lint` and `submit`; plan review checks them a second time before they become ready.
2. **A further Spike** with its own question and limit, in the goal: say what it settles that the earlier one could not. Never extend or chain a Spike only because its verdict is unresolved.
3. **An alternative**, or 4. **a smaller scope**: plan it as in 1, saying what the result ruled out.
5. **Stop**: `"$DAGQ" request decline N --reason '<why, from the result>'`.

The exit is not that nothing is unknown, but that what is left can be handled during the implementation. A goal has a limit of Spike cycles (the repository's setting): past it `submit` refuses a runtime planner's further Spike. Keep it a draft and ask a `planner_question --because scope` with your recommendation; a person decides another round. Ask too when the decision needs a person (the plan's intent, an acceptance) or your confidence is low (the `dagq-planner` skill's Basic policy).

## 5. While investigations are held

The repository may cap the Spikes running at once and the goals under investigation at once. A capped Spike waits `ready` (`candidates` says why) while implementation tasks take the slots (`reference/inspect.md`, "Priority and claim order"). These caps do not refuse `submit` and no planner waits for them, so do not hold your session, ready it another way, or raise it to `interrupt` to get past a limit.
