# The kinds of ask

Read this when an open ask's kind, options or effect is unclear (the `dagq-inbox` skill, step 3). Whatever the kind, show the ask as written and add no recommendation of your own.

- `approve_landing`: a review's doubt: `land` / `send_back` / `cancel`.
- `approve_plan`: plan review's concern: `ready` / `send_back: <reason>` / `cancel`.
- `decide`: a failed, interrupted or resume-exhausted run the recovery job could not fix: `retry` / `resume` / `cancel`, no `resume` once resumes are used up, plus the job's options, which send the run back to the job.
- `stalled`: a session idle without a receipt after one nudge (`reason: idle_without_receipt`), a typed text the session did not take (`reason: send_unconfirmed`), or a job's escalation; the question lists background work and screen: `wait` (the supervisor closes it, counts again), `intervene` (a person steps in), `propose`. It closes itself once the session moves on (`reference/status.md` of this skill).
- `worker_question`: typed into the worker's terminal, or sent as the next turn of a headless run (answered the same way). Its `topics` (in `status`' `asks` and the ask; the first is the primary) say what the worker left undecided, next to `reason_category`: show them with the question.
- `planner_question`: `adopt` / `cancel` / `keep_draft`.
- `answer_prompt`: a dialog the runtime and the job could not answer, or a resumed session's input box not ready (`input_not_ready`). No options: the answer is what to send, e.g. `enter`; the question ends with its screen.
- `stuck_exit`: a `/exit` neither could see through: `exit` / `wait`.
- `blocked`: the observer's, about one finding: `propose` (make it a proposal) / `dismiss`.
- `queue_hold`: login or cost, one per queue: `done` / `cancel_affected`. Claude's login ran out or it hit its usage limit, or no worker provider can be used (Claude held, and Codex missing or held). A run on a provider that cannot be used moves to the other provider by itself (`provider_switched`) and is in the ask only when it could not. Tell the person what to restore: log in again (`claude`, or `codex login` when the question names Codex) or wait for the limit's reset; once done, the answer is `done` (the supervisor releases every provider hold and continues the waiting runs and jobs). A Codex that cannot be used opens no ask of its own: `status`'s `supervisors[].provider_hold` shows it until its `retry_at`. What each provider event means: `${CLAUDE_PLUGIN_ROOT}/skills/dagq/reference/provider.md`.
- `update_failed` / `approve_update`: `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/update.md`.

The runtime applies `propose` (a planner of its own takes the finding) and `dismiss`. An answer the runtime does not apply comes back as `read the answer of ask <id> and close it` (the skill's step 4).
