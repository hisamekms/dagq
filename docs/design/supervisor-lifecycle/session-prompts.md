---
id: design-supervisor-lifecycle-session-prompts
type: design
title: "Session prompts"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1228-2
  - adr-t2159-1
  - adr-t1566-1
  - design-supervisor-lifecycle-prompt
  - design-supervisor-lifecycle
  - adr-0022
  - design-plugin-integration
  - design-supervisor-lifecycle-language
---

# Session prompts

inboxとplannerの初期promptは`src/application/prompt.rs`の`inbox_prompt(db)`とruntimeが立てるplannerの`runtime_planner_prompt`など（[`plan` / `planners`](plan-planners.md#plan--planners)）が生成し（worker promptと同じファイル。[Prompt](prompt.md#prompt)。`runtime`からも再公開）、`inbox_prompt`は5行以内（`runtime_planner_prompt`は指摘とtaskの行が足される。人が開くplannerの`planner_prompt`は`dagq plan`の廃止（ADR-t1394-1、task 1399）で消した。[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)、ADR-0016の決定8）。CLIの手順はpromptに書かずskillに置くので、skillを変えてもpromptは変わらない。compactionと`/clear`からの起き直しはpromptではなくpluginの`SessionStart` hookが役割とskillの1行に続けて`status --role <role>`を出して担う（[plugin-integration](../plugin-integration.md#起き直しhookadr-0016)）。inboxはaskを人に見せてanswerを書き戻し、runtimeが適用しないanswer（`stuck_exit`の`exit`、`answer_prompt`など）は人の指示として`dagq-recover` skillの`reference/session.md` / `reference/stuck-exit.md`に従って実行する（task 100。それまでは退役した常駐sessionが実行していた）。inboxのcommandは、人がterminalで打つ`dagq inbox`がactor executor（[Roles](roles.md#actorの起動actorexecutor)）で受け取りその前面でexecする、providerの`inbox_command`が作る`<claude> --settings <queueのディレクトリ>/claude-inbox-settings.json [--plugin-dir PATH] -- '<prompt>'`（promptは`lifecycle::inbox_session_prompt`。settingsは`permissions.deny`だけ。中身と`inbox_guardrail`は[`dagq inbox`](up-down.md#dagq-inbox)、[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定2）。runtimeのplannerはworkspaceを持たず、backgroundで起動したsession wrapper `planner-session`（`--headless --background`。task 1441）が`<planner dir>/prompt.txt`を最初のturnのpromptにして非対話の`claude`を起動する（[`plan` / `planners`](plan-planners.md#plan--planners)）。

- **inbox**: このqueue（db path）のinboxで、askとattentionを人に取り次ぎ自分では判断しないこと。`dagq status --role inbox`から始めてdagq pluginの`dagq-inbox` skillに従い、`dagq watch --role inbox --after <cursor>`をbackgroundで回して終了で起き、返ったcursorからwatchし直すこと。`ask_opened`が来たら`dagq asks --open --role inbox`でaskを読み、questionとoptionsを人に見せ（AskUserQuestionが使えるなら使う）、人の答えを`dagq answer ID --text '<answer>'`で書くこと。それ以外のattention（回答済みのask、止まったsupervisor、失敗したreview / triage）は人に知らせ、人の言うことだけをskillのとおり行うこと。queue DBを直接開かずCLIだけを使うこと。
- **planner**: 人が`dagq plan`で開いたplannerの初期prompt（`planner_prompt`）は、`dagq plan`の廃止（[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)、task 1399）で消した。`dagq plan`は何も開かず、inboxへの計画の依頼の案内を付けて拒む。plannerは全てruntimeが立て、その初期promptは次の段落の`runtime_planner_prompt`など。

runtimeが立てるplanner（`runtime_planner_prompt`・`draft_planner_prompt`・`finding_planner_prompt`・`request_planner_prompt`）のpromptの渡し方と上限は、headlessのjobと同じ[Prompt](prompt.md#headlessのjobのprompt)の「headlessのjobのprompt」の節が持つ（[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)）。渡し方はplannerのturnの`turn_command`のまま（task 1560は替えていない）で、4つのpromptはそれぞれ全体と節ごとの上限を持ち、決まった順で選び、省いた件数と読む方法（plannerのroleで打てる読むだけの`dagq`。`prompt::PLANNER_READS`）を書く（task 1571。値と理由は[Prompt](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)）。runtimeのplannerを開くとき（`launch_planner`）、言語の指示を足したpromptのbyte数をqueueのevent `planner_prompt_written`（`planner_id`、`subject: "planner"`、`prompt`: `runtime` / `draft` / `finding` / `request`、`prompt_bytes`）に記録する。人が開いたplanner（`dagq plan`の廃止の前に開いたもの）とinboxの初期promptはこの範囲に含めない。

言語の設定（`[language]`）が解決できるときは、どのroleの初期promptにも言語の指示の1行が足され（5行には数えない）、起き直しの`status --role`の出力にも同じ指示が載る（[Language](language.md#promptへの渡し方)、ADR-t616-2）。
