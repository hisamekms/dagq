# needs_session runs: what the runtime's resume does

A `needs_session` run is resumed by the supervisor itself (`resuming (runtime)`) and taken on from there, whatever parked it: a conflicted landing, a verification that failed after the rebase, missing evidence, a `send_back` answer to an `approve_landing` ask, or a replanning. None of it is yours: never open a resume workspace, never send anything to the session by hand, and do not `recover`, `ready` or `integrate` the run while the runtime holds it.

While a replanning holds the run's task, the run waits for the operation's outcome: read it with `"$DAGQ" replans` (`replans --help`; `reference/replan-held.md`). A restore of the saved work that failed shows there; bring it to the person.

What a resumed session adds goes through validation again, and before it lands the landing compares what it would merge (each touched file and its blob) with the run's last judgment that let it land. When they differ, nothing lands and that judgment no longer counts: the path that gave it approves again. A review's judgment is reviewed again, an `approve_landing` `land` has its ask opened again on the new diff, and a direct `integrate` waits as the `review and integrate` attention for `integrate` again. So a new `approve_landing` ask or a `review and integrate` attention may come.

What reaches the inbox is an ask or an attention, handled where it is described: a `decide` ask once the resumes ran out and the recovery job escalated, a `stalled` ask on a resumed session (`reference/stalled.md`), and an `answer_prompt` or `send_unconfirmed` ask, which is a past record (`reference/session.md`).
