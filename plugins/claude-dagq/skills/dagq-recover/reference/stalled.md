# A `stalled` ask: `wait`, `stop`, `propose`, and an instruction

Read this to carry out, on the person's word, the answer to a `stalled` ask (the `dagq-recover` skill, section 7).

## A headless run's `stalled` ask

A run's session is headless, with no screen and no workspace. Nothing is typed into that session, by hand or by any command: the answer is all it takes.

Before answering, read what the turns did (the question's last turns, `"$DAGQ" run log RUN`, `"$DAGQ" timeline RUN`, `"$DAGQ" events --run RUN --full`) and bring it to the person. Then answer with the person's choice; the supervisor applies it and closes the ask itself:

- **`wait`**: leave the session alone; the idle is counted again from the answer, so the same ask may come back.
- **`propose`** (or `propose: <why>`): a runtime planner proposes a remedy for the cause; the session is left alone as for `wait`.
- **`stop`**: the supervisor has the session exit; the run ends without a receipt and goes to its recovery job. Nobody ends the wrapper or signals a process by hand.
- **An option the job added** (marked `[goes back to the recovery job ...]`): nothing is sent to the session; the recovery job takes the person's answer the next time the alert comes.
- **An instruction** (any other text, not an option of the ask): sent as the session's next turn.

There is no `intervene`: one that arrives is not sent, and a new `stalled` ask opens; answer that one.

## Closing it

When it shows `read the answer of ask <id> and close it`, run `"$DAGQ" ask close <id>`; otherwise the supervisor closes it.

A past `stalled` ask with `reason: idle_without_receipt` or `send_unconfirmed` came from the retired interactive session: it is a record, and nothing is carried out for it.
