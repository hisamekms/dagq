# A worker's provider and route, and falling back

Read this to choose which agent runs a task's worker and how, to read which one actually ran, or to tell a person what to do when a provider cannot be used. The decisions are ADR-t813-1 (the headless route), ADR-t813-2 (provider per task, mutual fallback) and ADR-t813-3 (Codex's permissions); the runtime's side is `docs/design/provider-lifecycle.md` and `docs/design/supervisor-lifecycle/headless-worker.md`.

## Provider and route

A task's worker has a **provider** (`claude` or `codex`) and a **route** (`worker_mode`): `interactive` (a Claude Code session in the run's cmux terminal, read from its screen and idle marker, ended with `/exit`) or `headless` (one non-interactive call per turn; the call's exit and output are the turn's result, and an answer, a send-back or a resume is the next call on the same session id). Claude runs either way; Codex runs headless only. The task's provider changes only its worker. Three jobs can run on Codex, each only when its table in `dagq.toml` says `provider = "codex"` (a model, if given, is Codex's), and all run `codex exec --json` in the read-only sandbox. Each moves to Claude (`switched_from` and `switch_reason` in their start event's `launch`) while Codex cannot be used: Codex is missing, does not start, or is held after a job stopped at its login or usage limit (Codex's hold opens no ask). The goal review is `[roles.goal_review]` (ADR-t1063-1), the run's review `[roles.review]` (ADR-t1207-1), whose stopped Codex review records `review_retried` with `provider` and `switch_reason` and starts again on Claude, and the plan review `[roles.plan_review]` (task 1218), whose stopped Codex plan review records `plan_review_failed` with `provider_unusable` (no `plan review by hand`: the proposal is not held) and starts again on Claude. A Codex review or plan review does not wait for Claude's `queue_hold` ask. Under `--no-claude` none of them falls back to Claude: a run review with no provider left goes to a person in an `approve_landing` ask instead of waiting (`review_failed`), and a plan review with no provider left goes to `plan review by hand` with why (`plan_review_failed`). `provider = "codex"` for any other role is an error in `dagq.toml`. Recovery jobs, the observer, planners and the inbox stay on Claude. `dagq doctor`'s `roles` shows each role's provider and where it comes from.

| registered with | provider | route |
| --- | --- | --- |
| nothing | `claude` | `interactive` (the default) |
| `--headless` | `claude` | `headless` |
| `--provider codex` | `codex` | `headless` |
| `--provider codex --headless` | `codex` | `headless` (the same) |

`add` takes `--provider claude|codex` and `--headless`. `edit ID` changes them on a draft or submitted task (a ready one goes back with `draft ID` first): `--provider claude|codex` takes that provider's default route (so `--provider claude` alone is Claude interactive) unless the same `edit` says `--headless` or `--interactive`; `--headless` or `--interactive` alone keeps the provider. Codex interactive is refused.

### Which to choose (planner)

- No reason given: give neither flag. Claude interactive stays the default until a measuring task decides otherwise.
- The person wants the task done on Codex: `--provider codex`. Codex does no subagent review: its run needs no `subagent_review` evidence (the supervisor's review job still reviews it before landing). A Codex worker asks as any worker does: its `dagq ask` inside the sandbox runs in client mode and goes to the queue service, which opens the `worker_question` at once for the run's worker (no `ask_request_taken`; `queue-service.md`, client mode). Only a turn started before the queue service writes a request to the run dir (`ask-requests/`) for the supervisor to open.
- Claude headless (`--headless`): only when the person asks for it, for example to compare the routes.
- Never pick a provider to route around a login or a usage limit: the runtime falls back by itself (below).

## Reading which ran

- `show ID`: the task's `provider` and `worker_mode`; each run's `requested_provider` (the task's), `actual_provider` (the one running now, after any switch) and `worker_mode`.
- `timeline RUN`: `requested_provider`, `actual_provider`, `worker_mode` and `provider_switches` (each switch's payload with `event_id` and `at`).
- Events (`events --run RUN --full`): `run_claimed` has `provider`, `requested_provider`, `worker_mode`, `provider_version` and, when the supervisor runs Codex workers, `codex_version`. A headless run records each turn: `turn_requested` (`what`: `answer of ask N`, `revise request`, `nudge`, `provider switch`, `provider retry`, ...), `turn_started` (`turn`, `provider`, `session_id`), `turn_session_identified` (the thread id Codex named), and `turn_finished` (`outcome`, `failure`: `authentication` / `usage_limit` / `model` / `sandbox` / `launch` / `other`, `provider`, `usage`, `tokens`; Codex's `tokens_total` is the thread's running total). The turns' own output is under the run dir's `turns/`.
- A switch is `provider_switched`: `from`, `to`, `worker_mode`, `reason` (`executable_missing`, `launch_failed`, `authentication`, `usage_limit`), `phase` (`start` at the claim, or `answer` / `revise` / `resume` / `nudge` mid-run), `turn`, `count` (two at most: away, and back once), `message`. A run that could not switch records `provider_waiting` (`provider`, `reason`, `other`, `blocked`, `retry_at`) and waits; it is not failed.
- `stats`: `runs[]` `provider` (requested), `actual_provider`, `provider_switches`, `route`, `turns` (with `by_provider`); the window's `provider_switches` (`count`, `runs`, `by_reason`, `by_direction`, `by_phase`, `holds_by_reason`). `kpi --by provider|route|codex` (`reference/kpi.md`).
- `status` / `doctor`: `supervisors[].providers` (each provider's resolved `executable`, `found`, `error`, `modes`) and `supervisors[].provider_hold` (a provider held now, with `reason` and `retry_at`); `claim_deferrals` with `provider_unavailable` (neither provider can take the task) or `mode_unavailable`.

## When a provider cannot be used

Only these switch: the executable is missing (the supervisor found no `codex`; without Claude it does not start), the agent cannot start, a login is needed, or a usage limit or rate limit. A model error, a sandbox refusal, a stopped turn, a failed test or a `failed` receipt never switch: they go to the recovery job as before.

- **At the claim**: a task whose provider is missing or held is claimed on the other provider's headless route (`provider_switched`, `phase: start`). An interactive Claude task starts on Codex while Claude is held.
- **Mid-run**: the failed turn's request goes to a new session on the other provider, which reads the commits and changes made so far; the conversation is not carried over. An interactive Claude worker whose first session hits a login or usage limit opens Claude's `queue_hold` ask and, while headless Codex is usable and switches remain, is parked and resumed on headless Codex; otherwise it waits in that ask. Interactive sessions of a send-back or a resume are not switched.
- **Held**: Claude held for login or cost is the queue's `queue_hold` ask (below). Codex held, and a Claude that cannot start, open no ask: `provider_held` / `provider_released` queue events, shown in `status`'s `provider_hold`, released at `retry_at` (the reset time a usage-limit message names, else 10 to 30 minutes), when the next call checks again.

What reaches the person (the inbox shows it):

- **The `queue_hold` ask** (`authentication`, or `cost` with `subject: usage_limit`): Claude's login ran out or it hit its limit, or both providers are unusable. Claude-only jobs and runs that could not move wait in its `affected`. The person logs Claude (or Codex) in again or waits for the limit to reset, then answers `done` (the supervisor releases every hold, sends `continue` to the waiting runs and restarts the failed jobs) or `cancel_affected`. A Codex task already moved to Claude, or the other way, needs nothing.
- **Nothing opens** when both providers merely fail to start, or when a run used up its switches and waits for Codex's hold to end while Claude works: the runtime tries again at each hold's `retry_at`. If `status` keeps showing `provider_hold` or `provider_unavailable`, report it: the person checks the executable (`doctor`'s `providers`) and, for a missing or moved `codex`, restarts the supervisor with `--codex` (`skills/dagq-recover/reference/up-down.md`).

## A headless run by hand

A headless run's workspace runs only the session wrapper: there is no agent screen, no dialog and no input box, so nothing is typed into it and no `answer_prompt` or `stuck_exit` ask opens for it. A `worker_question` is asked and answered as usual; the supervisor sends `answer to ask N: ...` as the next turn. A `stalled` ask (`turn_without_receipt`, `permission_denied`) is answered `wait`, `propose`, `stop` (the supervisor has the session exit; the run ends without a receipt and goes to its recovery job), or with an instruction text, which becomes the next turn. It offers no `intervene`: the turns are read before answering, and an `intervene` that still arrives closes the ask and opens a new one (`skills/dagq-recover/reference/stalled.md`).
