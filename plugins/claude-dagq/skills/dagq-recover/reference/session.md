# A run's session: dialogs, stuck exits and undelivered answers

Read this to carry out, on the person's word, an answer that has to reach a run's Claude session in its cmux workspace (the `dagq-recover` skill, section 7). A run's session works only in its own worktree; never edit that worktree, merge or push for it, and never `recover` a run whose session only waits at a dialog.

Take `workspace_id`, `worktree_path`, the run `id` and `last_error` from `"$DAGQ" show ID` (add `--full` when `last_error` is cut at 300 characters). A run's workspace is named `[<repo>]worker#<task-id> - <task title>` with the description `dagq role=worker queue=<queue hash> run=<run-id> task=<id>`; a resumed session's is titled the same with the description `run <run-id> resume`. Names are for people; the runtime finds workspaces by `workspace_id`.

## cmux commands

```sh
cmux read-screen --workspace <workspace_id> --lines 40   # read before sending anything
cmux send-key --workspace <workspace_id> down            # move to the choice you want
cmux send-key --workspace <workspace_id> enter           # confirm a dialog, or submit text
cmux send --workspace <workspace_id> "<text>"            # type an answer or /exit, then send-key enter
```

- A dialog is answered with keys, not with text: send `down` only until the choice you want is highlighted, then `enter`.
- The runtime closes the workspaces of accepted runs and of runs its recovery job handled itself, and the supervisor sweeps whatever cmux still lists of ended runs (landed by hand, superseded, or whose task moved on) within a minute; `cmux workspace close <workspace_id>` is only for a run whose recovery job failed (`triage by hand`) while its task is still in progress, or when no supervisor runs.
- The folder-trust prompt is decided by the repository root, not the worktree. Running `claude` once in the repository root before the first run avoids it.
- Never open a resume workspace yourself; only a dialog a resumed session stops at is answered here.

## An `answer_prompt` ask (a dialog)

When a run has been `running` for 90 seconds with no receipt and no idle marker, the supervisor reads its screen. A known dialog it answers itself when its conditions hold (the Settings panel; "Background work is running" only after its `/exit`, with the worktree clean and the receipt at HEAD), recorded as `auto_repaired` (`repair: dialog_answered`). Any other dialog (trust, an LSP plugin recommendation, the auto mode notice, any `❯`-marked numbered choice or `Enter to confirm` / `Esc to cancel` footer) is recorded once as `prompt_waiting` and handed to the recovery job (alert `prompt_waiting`), which may answer a known dialog, stop the run's processes or wait. Only when the job escalates, is not sure, finds its preconditions broken or used its three tries is it raised as an `answer_prompt` ask (`asked_by` `supervisor`, `reason_category` `recovery_failed` unless the job named `scope` or `discard`) whose question names the workspace, the job's diagnosis and suggested actions, and ends with the last 15 lines of the screen. From then on the supervisor sends no key. Once the person answered it (`read the answer of ask <id> and close it`), read the screen of the named workspace, send the chosen key or text as the answer says, and `"$DAGQ" ask close <id>`. When the dialog leaves the screen, the receipt arrives or the session exits, the supervisor closes the ask itself (`answer` then reports it is not open). A run with an unclosed `worker_question` is not read for a dialog: it waits for its answer.

While either ask is open the run usually holds no `--parallel` slot (`status`'s `waiting`, ADR-0071), and keys you send reach its session as usual. A dialog you answered ends an `answer_prompt` wait at once (`dialog_cleared`, or `session_moved` when the session wrote a new idle marker, prompt or receipt); while `/exit` waits, or while a send-back or resume also holds an unanswered `worker_question`, the screen is not checked and only a new marker (not during that `worker_question`) or the session's exit ends it; an answered `worker_question` makes the run `returning` until a slot is free, and the supervisor types the answer only then. Do not type a worker's answer yourself while its entry shows `returning`: that is the runtime's delivery, not an undelivered answer.

## An undelivered worker answer

A worker's `worker_question` is answered by the person in the inbox; the supervisor types `answer to ask <id>: <answer>` and Enter into the worker's terminal once the worker went idle after asking, closes the ask and records `ask_delivered` (`delivering the answer of ask <id> (runtime)` meanwhile). Attention `send the answer of ask <id> to the worker and close it` (`kind` `ask_delivery_failed`, or `ask_answered` on a run whose session no longer takes answers: not `running`, and not in a review's send-back or a resume): the supervisor could not type it (it tries once), or the session is gone. When the session still works (`running`, a send-back, a resume), read the worker's screen, send the text `answer to ask <id>: <answer>` followed by `enter`, then `"$DAGQ" ask close <id>`. When the run is at rest, the answer has nobody to reach: tell the person and close the ask.

## A `stuck_exit` ask

The answer `exit` is carried out as `reference/stuck-exit.md` says; `wait` needs only `ask close <id>`.

## `recover by hand` (a live run's recovery job failed)

Attention `recover by hand` (`kind` `recovery_failed`): the recovery job for a live run's alert (`long_background`, `idle_process`, `stuck_exit`, `prompt_waiting`) could not start, exited non-zero, timed out or printed no verdict. No ask opens and the job does not run again for that alert; the session is left as it is. Read the screen, show it to the person with `last_error` and the job's files in the run directory (`recovery-<alert>-<attempt>.prompt.txt`, `.out`, `.err`), and do what they say: answer a dialog as for `answer_prompt` above, or for a `stuck_exit` follow the `exit` steps of `reference/stuck-exit.md`. The attention clears once the session exits or its workspace closes (a `prompt_waiting` one also when the dialog goes, a `long_background` one when the receipt arrives).
