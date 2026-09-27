# A run's session: dialogs, stalls, stuck exits and undelivered answers

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
- Never open a resume workspace yourself; in a resumed session only a dialog, an `input_not_ready` answer or a `send_unconfirmed` `intervene` is carried out here.

## An `answer_prompt` ask (a dialog)

When a run has been `running` for 90 seconds with no receipt and no idle marker, the supervisor reads its screen. A known dialog it answers itself when its conditions hold (the Settings panel; "Background work is running" only after its `/exit`, with the worktree clean and the receipt at HEAD), recorded as `auto_repaired` (`repair: dialog_answered`). Any other dialog (trust, an LSP plugin recommendation, the auto mode notice, any `❯`-marked numbered choice or `Enter to confirm` / `Esc to cancel` footer) is recorded once as `prompt_waiting` and handed to the recovery job (alert `prompt_waiting`), which may answer a known dialog, stop the run's processes or wait. Only when the job escalates, is not sure, finds its preconditions broken or used its three tries is it raised as an `answer_prompt` ask (`asked_by` `supervisor`, `reason_category` `recovery_failed` unless the job named `scope` or `discard`) whose question names the workspace, the job's diagnosis and suggested actions, and ends with the last 15 lines of the screen. From then on the supervisor sends no key. Once the person answered it (`read the answer of ask <id> and close it`), read the screen of the named workspace, send the chosen key or text as the answer says, and `"$DAGQ" ask close <id>`. When the dialog leaves the screen, the receipt arrives or the session exits, the supervisor closes the ask itself (`answer` then reports it is not open). A run with an unclosed `worker_question` is not read for a dialog: it waits for its answer.

While either ask is open the run usually holds no `--parallel` slot (`status`'s `waiting`, ADR-0071), and keys you send reach its session as usual. A dialog you answered ends an `answer_prompt` wait at once (`dialog_cleared`, or `session_moved` when the session wrote a new idle marker, prompt or receipt); while `/exit` waits, or while a send-back or resume also holds an unanswered `worker_question`, the screen is not checked and only a new marker (not during that `worker_question`) or the session's exit ends it; an answered `worker_question` makes the run `returning` until a slot is free, and the supervisor types the answer only then. Do not type a worker's answer yourself while its entry shows `returning`: that is the runtime's delivery, not an undelivered answer.

## An ask about a text the session did not take (`input_not_ready`, `send_unconfirmed`)

The supervisor checks the screen after everything it types (the runtime's side: `docs/design/supervisor-lifecycle/session-send.md`). Two asks come from a text or request a session did not take:

- **`answer_prompt` with no options, `input_not_ready`**: a resumed session's input box did not get ready for the resolution request within the agent's registration timeout, often because a dialog holds it (the question then says `a <kind> dialog holds the resumed session`). The request is not typed yet: once the box has been ready for a few seconds the supervisor closes the ask (`the input box got ready and the request was sent; closed by the runtime`) and types the request itself. The answer says what to send, for example `enter`, a dialog's choice, or a text.
- **`stalled` with `reason: send_unconfirmed`, answered `intervene`**: a text the supervisor typed (a resolution request, a worker's answer, a send-back, a nudge) stayed in the input box after four Enters (`submit_unconfirmed`), or the session showed no sign of work `[stall].send_confirm_secs` after it (`submit_not_started`: a dialog came up, the text stays in the box, or it was lost), and the recovery job could not fix it. A dialog that came up is answered under this ask; no `answer_prompt` opens for it. `wait` here leaves the session alone without counting again: no new `stalled` ask follows for the same send. The question names the text (`the resolution request ...`, `the answer ...`) and ends with the screen.

Carry out the answer on the person's word, in the workspace the question names:

1. Read the screen: `cmux read-screen --workspace <workspace_id> --lines 40`. If the session is already at work, or the dialog or the text is gone, send nothing: the supervisor closes the ask itself (`answer` or `ask close` then reports it is not open).
2. Send what the answer says, and nothing else:
   - `enter`: the text sits in the input line; `cmux send-key --workspace <workspace_id> enter` once.
   - a dialog's choice: answer it with keys as for an `answer_prompt` dialog above (`down` until it is highlighted, then `enter`).
   - a text (the person's own, or the supervisor's text typed again when it was lost): `cmux send --workspace <workspace_id> "<text>"`, then `cmux send-key --workspace <workspace_id> enter`. A worker's answer is typed as `answer to ask <id>: <answer>`. Never type the resolution request for `input_not_ready`: the supervisor sends it itself.
3. Read the screen again (`--lines 20`): the text left the input line and the session works. If it still sits there, send `enter` once more; if a dialog came up, show it to the person.
4. `"$DAGQ" ask close <id>`, unless the supervisor closed it first. The supervisor takes a person's input as the session's input and watches the session from there.

## An undelivered worker answer

A worker's `worker_question` is answered by the person in the inbox; the supervisor types `answer to ask <id>: <answer>` and Enter into the worker's terminal once the worker went idle after asking, closes the ask and records `ask_delivered` (`delivering the answer of ask <id> (runtime)` meanwhile). Attention `send the answer of ask <id> to the worker and close it` (`kind` `ask_delivery_failed`, or `ask_answered` on a run whose session no longer takes answers: not `running`, and not in a review's send-back or a resume): the supervisor could not type it (it tries once), or the session is gone. When the session still works (`running`, a send-back, a resume), read the worker's screen, send the text `answer to ask <id>: <answer>` followed by `enter`, then `"$DAGQ" ask close <id>`. When the run is at rest, the answer has nobody to reach: tell the person and close the ask.

## A `stuck_exit` ask

The answer `exit` is carried out as `reference/stuck-exit.md` says; `wait` needs only `ask close <id>`.

## A `stalled` ask

A session idle without a receipt after the supervisor's one nudge, or a `long_background` / `idle_process` alert its recovery job escalated. The supervisor applies and closes `wait` itself, and closes the ask once the session moves on or exits. `intervene` is carried out as `reference/stalled.md` says: read the screen, check the background work, type the person's instruction or `/exit` the session, then `ask close <id>`.

## A live run's recovery job failed

When the recovery job for a live run's alert (`long_background`, `idle_process`, `stuck_exit`, `prompt_waiting`, `stalled`) could not start, exited non-zero, timed out or printed no verdict, the supervisor opens the alert's own ask (`stalled`, `stuck_exit` or `answer_prompt`, with its usual options), with `reason_category` `recovery_failed` and `the recovery job failed (<error>)` in the question (ADR-t609-1). Carry out its answer as for that kind above; no job runs for the alert while it is open, and it closes as that kind does.

Attention `recover by hand` (`kind` `recovery_failed`) is such a failure a runtime from before ADR-t609-1 recorded: no ask opened and the job does not run again for that alert; the session is left as it is. Read the screen, show it to the person with `last_error` and the job's files in the run directory (`recovery-<alert>-<attempt>.prompt.txt`, `.out`, `.err`), and do what they say: answer a dialog as for `answer_prompt` above, or for a `stuck_exit` follow the `exit` steps of `reference/stuck-exit.md`. The attention clears once the session exits or its workspace closes (a `prompt_waiting` one also when the dialog goes, a `long_background` one when the receipt arrives).
