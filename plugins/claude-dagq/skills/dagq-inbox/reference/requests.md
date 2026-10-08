# A plan the person asks for: hand it to a runtime planner

Read this when the person asks the inbox for new work or a change of plan (a goal, tasks, a fix of something they saw, "plan this", "drop that goal"), and when `report the request's proposal` or `rephrase or drop the request` reaches you. People no longer open planners: `dagq plan` is refused with a pointer here. The inbox does not plan either (no `add`, `goal add`, `edit` or `submit`): it records the person's words as a **planning request**, and the supervisor opens a planner of its own (the `dagq-planner` skill) that plans it or declines it without a person, asking only what a person must decide.

## Record the request

On the person's word:

```sh
"$DAGQ" request add --text '<the person's own words>' [--note '<what you add>'] [--ref task:N ...] [--priority LEVEL]
```

- `--text` holds the person's own words, not your summary. Give them exactly one way: `--text '<words>'`, `--text-file PATH` (what the file holds when you run it is recorded, not its path; changing the file later changes nothing) or `--text -` (read from stdin). Long words, several lines or words with quotes break in a shell's quoting: write them to a file or pass them on stdin, as below. Two of them, or none, is refused.
- `--note` is what you add (what you read in `status`, an ask's context), kept apart from their words. It takes the same three forms (`--note`, `--note-file PATH`, `--note -`), at most one; only one of `--text -` and `--note -` can read stdin.
- A file or stdin that cannot be read, is not UTF-8 or holds nothing (or only blanks) is refused with the reason, and nothing is recorded. The words are kept as given: line breaks, quotes and Japanese unchanged.
- `--priority LEVEL` (`interrupt`, `urgent`, `high`, `normal` or `low`) only when the person's words give the request a priority; words that give none get none, and you do not infer one from the subject or the repository's guide. The runtime gives that value to the goals the request's planner makes as the person's (`priority_by: human`), which only the person and the inbox can change; the planner does not copy a priority from the words. Words that name different priorities for different parts: leave it out and, once the plan is in, set each on the person's word (`goal edit ID --priority`, `set-priority`).
- `--ref KIND:ID` (repeatable) names what they refer to: `ask:N`, `task:N`, `run:ID`, `event:N`, `finding:N` or `goal:N`. The planner's prompt carries each one (an ask's question and answer, a task's acceptance and landed receipt, a finding's detail) and the goals they lead to. The three attentions `request a plan for ...` name the draft, finding or stranded task to refer to (`reference/status.md`).

Long words through a file or stdin (a quoted heredoc delimiter keeps `$`, backticks and quotes as written):

```sh
cat > "$TMPDIR/request.md" <<'WORDS'
<the person's own words, as many lines as they wrote>
WORDS
"$DAGQ" request add --text-file "$TMPDIR/request.md" --ref goal:N

"$DAGQ" request add --text - --note 'asked in the inbox after the 10:00 report' --priority urgent <<'WORDS'
<the person's own words, which call it urgent>
WORDS
```

It prints the request (`id`, `status` `open`). Tell the person its ID. The person may type the same command in a terminal without `DAGQ_ROLE` (recorded as `user` instead of `inbox`). A planner, a worker, a job and the observer are refused it.

## What the runtime does with it

The next supervisor pass opens a runtime planner for each `open` request, before draft and finding planners, within `[supervisor] runtime_planners`, which those planners share; with no supervisor running it waits (`up`, `dagq-recover` section 5). A request that names a draft (`--ref task:N`) a runtime planner already works on (a draft's planner, one its revisit time opened included) waits until that planner ends; the other way round, while an `open` request names a draft, no draft planner opens for it, and the draft's revisit time, if it has one, waits for the request's planner (`${CLAUDE_PLUGIN_ROOT}/skills/dagq/reference/register.md`, "A draft's revisit time"). The planner runs headless in the background, each step a turn (there is no route to choose; an old `route` key in `dagq.toml` is ignored), and reads the words and the referred records, looks for work that already covers them, and does one of:

- **plans it**: drafts a goal or tasks with context `from request N` and submits them; the request becomes `proposed`. Plan review then checks the proposal as any other (a new goal needs no person's approval); a concern of plan review reaches you as its `approve_plan` ask.
- **declines it** with a reason (done already, a duplicate of work in flight, not plannable as asked); the request becomes `declined`.
- **asks** a `planner_question` (below).

A planner that ends without deciding is replaced at the next pass, at most 3 per request; then the request is `exhausted`. A planner whose only wait is the person's answer to its question is ended to free its place (`planner_closed` with `code` `runtime_answer_wait`; the ask stays open) and is not counted; the planner the answer opens counts only if it too ends without deciding.

Follow them with `"$DAGQ" requests` (the open ones), `requests ID` (one, whatever its status) or `requests --all`: each has `text`, `note`, `refs`, `requested_by`, `status` (`open`, `proposed`, `declined`, `exhausted`), `status_reason`, its `proposals` and the planners opened for it. `dagq planners` lists the planner sessions not closed.

## Tell the person the outcome

`watch` brings each outcome (queue events on no task; `status` does not list them, `"$DAGQ" events --kind request_proposed` and the like read them again):

- `report the request's proposal` (`request_proposed`): tell the person the request and the proposal (`"$DAGQ" proposal show ID`, its tasks with `show`). Nothing else waits on them: plan review takes it from here, and the tasks run once it passes.
- `rephrase or drop the request` (`request_declined` with its `reason`, or `request_planner_exhausted` after 3 planners that ended without deciding; one ended only to wait for the person's answer is not one of them): show the person the reason. On their word record a new request in other words, naming the old one's references again (and what the reason pointed at); or leave it, which needs no command.

## A planner_question about a request

The planner asks with `ask --request N --kind planner_question`: options `plan` / `decline`, its `recommendation` and `confidence`, and a question holding what the person needs. Show it as every ask (the skill's step 3) and write the person's answer as given. The runtime delivers the answer to that planner as its next turn, or, when it is gone, opens a new planner for the request with the answer in its prompt. A planner that waits only for the answer is ended meanwhile, and the request gets no planner until the answer: the new one carries the question, the answer and what the ended planner noted and drafted. Do not carry it out yourself.

## More words for a planner still at work

As the skill says, a follow-up to an open headless runtime planner goes as its next turn with `"$DAGQ" planner request <planner id> --text '<the person's words>'` (the planner id from `dagq planners` or the request's planners). The words take the same three forms as `request add`, exactly one: `--text '<words>'`, `--text-file PATH` (`--file` is the same; what the file holds when you run it is written, not its path) or `--text -` (stdin). Two of them, or none, is refused, and so is a file or stdin that cannot be read, is not UTF-8 or holds nothing (or only blanks), with the reason; nothing reaches the planner then. Long or multi-line words go through a file or stdin:

```sh
"$DAGQ" planner request <planner id> --text - <<'WORDS'
<the person's own words, as many lines as they wrote>
WORDS
```

It is refused for an interactive planner and for one closed, lost, exited or asked to exit; then, or for a new plan, record a new request.
