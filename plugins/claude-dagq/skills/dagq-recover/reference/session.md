# A run's session: dialogs, stalls, stuck exits and undelivered answers

Read this to carry out, on the person's word, an answer that has to reach a run's Claude session in its cmux workspace (the `dagq-recover` skill, section 7). Reach the session only through the `dagq run` commands below, never with `cmux` itself: they find the workspace from the queue, are judged by the actor's policy and are recorded with the actor; the inbox's settings refuse `cmux`. A run's session works only in its own worktree; never edit that worktree, merge or push for it, and never `recover` a run whose session only waits at a dialog.

Take the run `id`, `worktree_path` and `last_error` from `"$DAGQ" show ID` (add `--full` when `last_error` is cut at 300 characters); the commands take the run id (or the task id, for its latest run), never a workspace UUID. A run's workspace is named `[<repo>]worker#<task-id> - <task title>` with the description `dagq role=worker queue=<queue hash> run=<run-id> task=<id>`; a resumed session's is titled the same with the description `run <run-id> resume`. Names are for people; the runtime finds workspaces by `workspace_id`.

Every run's worker is on the headless route, Claude's and Codex's: `add` and `edit` refuse `--interactive`, and a run `show` gives as `worker_mode: interactive`, recorded before that route was retired, is claimed and resumed headless. Such a run has no agent screen: each turn is one non-interactive call. The screen and key steps below are for an interactive session, which no run starts any more. For a run, `run screen` names its `turns/` directory instead of a screen, `run send` refuses it, no `answer_prompt` or `stuck_exit` ask opens for it, and the supervisor sends a worker's answer as the next turn. Its `stalled` ask is `reference/stalled.md`, "A headless run's `stalled` ask"; the route and fallbacks are `skills/dagq/reference/provider.md`. A headless session runs in the background with no workspace at all (`background` in `show`): read what its turns did with `"$DAGQ" run log RUN` (`--follow` while it runs; `planner log ID` for a planner), as `skills/dagq/reference/provider.md`, "A headless run by hand", describes.

## The commands

```sh
"$DAGQ" run screen RUN --lines 40                # read before sending anything (at most 200 lines)
"$DAGQ" run send RUN --key down --key enter      # keys of the set: enter, escape, up, down, 1-9
"$DAGQ" run send RUN --key exit                  # /exit, alone
"$DAGQ" run send RUN --answer ASK_ID             # types `answer to ask <id>: <answer>` of an answered ask on this run
"$DAGQ" run close-workspaces [RUN] [--apply]     # ended runs' workspaces; a dry run without --apply
```

- `run send` types no free text: only the keys of the set, `/exit`, or the answer of an answered ask on that run. A person's own words reach the session as an ask's answer: write them with `answer` first, then `--answer`.
- A dialog is answered with keys, not with text: `down` only until the choice you want is highlighted, then `enter` (or the choice's number).
- The runtime closes the workspaces of accepted runs and of runs its recovery job handled itself, and the supervisor sweeps whatever cmux still lists of ended runs (landed by hand, superseded, or whose task moved on) within a minute; `run close-workspaces` lists what it would close and closes it with `--apply`, also without a supervisor; `run close-workspaces RUN --apply` also closes a run whose recovery job failed (`triage by hand`) once nothing lives behind it. It never closes a live run's workspace.
- The folder-trust prompt is decided by the repository root, not the worktree. Running `claude` once in the repository root before the first run avoids it.
- Never open a resume workspace yourself; in a resumed session only a dialog, an `input_not_ready` answer or a `send_unconfirmed` `intervene` is carried out here.

## An `answer_prompt` ask (a dialog)

When a run has been `running` for 90 seconds with no receipt and no idle marker, the supervisor reads its screen. A known dialog it answers itself when its conditions hold (the Settings panel; "Background work is running" only after its `/exit`, with the worktree clean and the receipt at HEAD), recorded as `auto_repaired` (`repair: dialog_answered`). Any other dialog (trust, an LSP plugin recommendation, the auto mode notice, any `❯`-marked numbered choice or `Enter to confirm` / `Esc to cancel` footer) is recorded once as `prompt_waiting` and handed to the recovery job (alert `prompt_waiting`), which may answer a known dialog, stop the run's processes or wait. Only when the job escalates, is not sure, finds its preconditions broken or used its three tries is it raised as an `answer_prompt` ask (`asked_by` `supervisor`, `reason_category` `recovery_failed` unless the job named `scope` or `discard`) whose question names the workspace, the job's diagnosis and suggested actions, and ends with the last 15 lines of the screen. From then on the supervisor sends no key. Once the person answered it (`read the answer of ask <id> and close it`), read the run's screen (`run screen`), send the chosen keys as the answer says (`run send RUN --key ...`; a text only as the ask's answer, `run send RUN --answer <id>`), and `"$DAGQ" ask close <id>`. When the dialog leaves the screen, the receipt arrives or the session exits, the supervisor closes the ask itself (`answer` then reports it is not open). A run with an unclosed `worker_question` is not read for a dialog: it waits for its answer.

While either ask is open the run usually holds no `--parallel` slot (`status`'s `waiting`), and keys you send reach its session as usual. A dialog you answered ends an `answer_prompt` wait at once (`dialog_cleared`, or `session_moved` when the session wrote a new idle marker, prompt or receipt); while `/exit` waits, or while a send-back or resume also holds an unanswered `worker_question`, the screen is not checked and only a new marker (not during that `worker_question`) or the session's exit ends it; an answered `worker_question` makes the run `returning` until a slot is free, and the supervisor types the answer only then. Do not type a worker's answer yourself while its entry shows `returning`: that is the runtime's delivery, not an undelivered answer.

## An ask about a text the session did not take (`input_not_ready`, `send_unconfirmed`)

The supervisor checks the screen after everything it types. Two asks come from a text or request a session did not take:

- **`answer_prompt` with no options, `input_not_ready`**: a resumed session's input box did not get ready for the resolution request within the agent's registration timeout, often because a dialog holds it (the question then says `a <kind> dialog holds the resumed session`). The request is not typed yet: once the box has been ready for a few seconds the supervisor closes the ask (`the input box got ready and the request was sent; closed by the runtime`) and types the request itself. The answer says what to send, for example `enter`, a dialog's choice, or a text.
- **`stalled` with `reason: send_unconfirmed`, answered `intervene`**: a text the supervisor typed (a resolution request, a worker's answer, a send-back, a nudge) stayed in the input box after four Enters (`submit_unconfirmed`), or the session showed no sign of work `[stall].send_confirm_secs` after it (`submit_not_started`: a dialog came up, the text stays in the box, or it was lost), and the recovery job could not fix it. A dialog that came up is answered under this ask; no `answer_prompt` opens for it. `wait` here leaves the session alone without counting again: no new `stalled` ask follows for the same send. The question names the text (`the resolution request ...`, `the answer ...`) and ends with the screen.

Carry out the answer on the person's word, on the run the question names:

1. Read the screen: `"$DAGQ" run screen RUN --lines 40`. If the session is already at work, or the dialog or the text is gone, send nothing: the supervisor closes the ask itself (`answer` or `ask close` then reports it is not open).
2. Send what the answer says, and nothing else:
   - `enter`: the text sits in the input line; `"$DAGQ" run send RUN --key enter` once.
   - a dialog's choice: answer it with keys as for an `answer_prompt` dialog above (`--key down` until it is highlighted, then `--key enter`).
   - a text: only the person's own words written as this ask's answer, `"$DAGQ" run send RUN --answer <id>` (it types `answer to ask <id>: <answer>` and Enter, and checks it was taken). A text of the supervisor's that was lost is typed the same way: the person's answer to this ask is that text (written with `answer` when answering), then `run send RUN --answer <id>`. An answer without the text (`intervene` alone) has nothing to send: stop the run (`run send RUN --key exit`) or leave it, on the person's word. Never type the resolution request for `input_not_ready`: the supervisor sends it itself.
3. Read the screen again (`run screen RUN --lines 20`): the text left the input line and the session works. If it still sits there, `run send RUN --key enter` once more; if a dialog came up, show it to the person.
4. `"$DAGQ" ask close <id>`, unless the supervisor closed it first. The supervisor takes a person's input as the session's input and watches the session from there.

## An undelivered worker answer

A worker's `worker_question` is answered by the person in the inbox; the supervisor types `answer to ask <id>: <answer>` and Enter into the worker's terminal once the worker went idle after asking, closes the ask and records `ask_delivered` (`delivering the answer of ask <id> (runtime)` meanwhile). Attention `send the answer of ask <id> to the worker and close it` (`kind` `ask_delivery_failed`, or `ask_answered` on a run whose session no longer takes answers: not `running`, and not in a review's send-back or a resume): the supervisor could not type it (it tries once), or the session is gone. When the session still works (`running`, a send-back, a resume), read the worker's screen (`run screen RUN`) and `"$DAGQ" run send RUN --answer <id>`: it types `answer to ask <id>: <answer>` with Enter and closes a `worker_question` as delivered; close any other with `"$DAGQ" ask close <id>`. When the run is at rest, the answer has nobody to reach: tell the person and close the ask.

## A `stuck_exit` ask

The answer `exit` is carried out as `reference/stuck-exit.md` says; `wait` needs only `ask close <id>`.

## A `stalled` ask

A headless session whose turn ended without a receipt after the supervisor's nudges and its recovery job (or again after a `wait`) (`turn_without_receipt`), whose tools were refused too often (`permission_denied`), or an `idle_process` alert its recovery job escalated. It has no screen and takes no keys: read what its turns did and answer `wait`, `stop`, `propose` (or an option the job added), or the person's instruction as the answer's text, which the supervisor sends as the session's next turn. The supervisor applies every answer and closes the ask itself; an answer starting with `intervene` is not sent, and the supervisor opens the ask again without it. The steps: `reference/stalled.md`, "A headless run's `stalled` ask".

## A live run's recovery job failed

When the recovery job for a live run's alert (`long_background`, `idle_process`, `stuck_exit`, `prompt_waiting`, `stalled`) could not start, exited non-zero, timed out or printed no verdict, the supervisor opens the alert's own ask (`stalled`, `stuck_exit` or `answer_prompt`, with its usual options), with `reason_category` `recovery_failed` and `the recovery job failed (<error>)` in the question. Carry out its answer as for that kind above; no job runs for the alert while it is open, and it closes as that kind does.

Attention `recover by hand` (`kind` `recovery_failed`) is such a failure an older runtime recorded, from before a failed recovery job opened an ask: no ask opened and the job does not run again for that alert; the session is left as it is. Read the screen (`run screen RUN`), show it to the person with `last_error` and the job's files in the run directory (`recovery-<alert>-<attempt>.prompt.txt`, `.out`, `.err`), and do what they say: answer a dialog as for `answer_prompt` above, or for a `stuck_exit` follow the `exit` steps of `reference/stuck-exit.md`. The attention clears once the session exits or its workspace closes (a `prompt_waiting` one also when the dialog goes, a `long_background` one when the receipt arrives).
