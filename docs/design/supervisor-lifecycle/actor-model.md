---
id: design-supervisor-lifecycle-actor-model
type: design
title: "Actor model"
status: current
created: 2026-09-27
updated: 2026-09-29
last_verified: 2026-09-29
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-worker-model
  - design-supervisor-lifecycle-plan-review
  - design-supervisor-lifecycle-goal-review
  - design-supervisor-lifecycle-throughput-review
  - design-supervisor-lifecycle-plan-planners
  - design-provider-lifecycle
  - adr-0079
---

# Actor model

worker以外のアクターのsessionのmodelとeffortの設定と、provider・model・effortの記録（[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)の決定7の(b)(c)、task 580）。workerは[Worker model](worker-model.md)。計測（transcriptから読む実際のmodel / effort）は[Agent provider lifecycle](../provider-lifecycle.md#modelとeffort)。

## 役割と`[roles.<role>]`

`dagq.toml`の`[roles.<role>]`で役割ごとに`model`（空でない文字列）と`effort`（`low` / `medium` / `high` / `xhigh` / `max`）を置ける（書式は[Run environment](run-environment.md)、解析は`infrastructure::run_env::parse_config`、型は`domain::actor_model::RoleModels`）。

| role | sessionと起動する場所 | 読む時点 |
|---|---|---|
| `plan_review` | plan reviewのjob（`Supervisor::start_plan_review`） | jobの開始ごと |
| `review` | runのreviewのjob（`begin_review`） | jobの開始ごと |
| `recovery` | 復旧（triage）のjob。終わったrunの`begin_triage`と、生きているrunの復旧のjob | jobの開始ごと |
| `goal_review` | goal reviewのjob（`Supervisor::start_goal_review`、task 1062） | jobの開始ごと |
| `observer` | observerのjob（`dagq observe`。queueが束縛されたcheckoutの`dagq.toml`） | 観測ごと |
| `throughput_review` | スループットの見直しのjob（`dagq throughput-review`。queueが束縛されたcheckoutの`dagq.toml`、task 997） | 見直しごと |
| `runtime_planner` | runtimeが立てるplanner（reviseのplanner、draftのplanner、findingのplanner） | plannerを開くpassごと |
| `planner` | 人が`dagq plan`で開くplanner（main checkoutの`dagq.toml`） | `dagq plan`ごと |

- **無いとき**: 役割の表が無い（keyの無い表も同じ）ときは今までと同じ起動で、`--model` / `--effort`を渡さない（Claude Codeの既定。今はOpus 5.5・medium）。この task を入れただけでは挙動は変わらない
- **あるとき**: 表の`model`と`effort`を`AgentProvider::apply_launch`が`AgentProvider::select_model`（task 576の仕組み。Claude Codeでは`--model <model> --effort <effort>`をoptionの末尾、`--`の前）で渡す。片方だけ書いたときは、もう片方は既定（`claude-opus-5-5` / `medium`）を明示して渡す
- **読めないとき**: supervisorとobserverはwarnをlogに出して今までと同じ起動にする（壊れた`dagq.toml`はprovisioningがerrorにする）。`dagq plan`は結果の`warnings`に足して今までと同じ起動でplannerを開く（`[roles]`の外の誤りや新しいバイナリだけが知る表で、人のplannerを開けなくしない）
- 設定は読む時点ごとに読み直すので、supervisorの再起動なしに次のjobから効く
- この repositoryの`dagq.toml`には役割の表を置かない（highに上げるのは基準値がたまってから人とplannerが決める。ADR-0079の決定7の(d)）。旧バイナリは`[roles.*]`を未知の表として拒むので、足すのはそれを知るバイナリに入れ替えた後にする

## 差し戻しで開き直すplannerの段上げ

plan reviewの`revise`（と`reopen`）の指摘を配るとき、持ち主のplannerが居ないのでruntimeが新しくplannerを立てる（`open_runtime_planner`、[Plan review](plan-review.md)の9）と、そのplannerのeffortを`[roles.runtime_planner]`の値（無ければ`medium`）から1段上げる（`domain::actor_model::raise`: `low` → `medium` → `high` → `xhigh`、`xhigh`が上限で同じ段のまま。`max`は上げない）。modelは表の値（無ければ`claude-opus-5-5`を明示）のまま変えない。理由（`escalation_reason`）は`plan_review_revise`。

生きているplannerへの配送（`submit_input`で指摘を打ち込む）は新しく起動しないので上げない。runtimeは動いているsessionの中でmodel / effortを切り替えないので、`plan_revise_sent`に上げなかった理由（`effort_not_raised`）を残す。

## 記録（`launch`）

起動したprovider・model / effortと、model / effortの出どころを`launch`（`domain::actor_model::ActorLaunch`）として記録する。

```json
{"role": "runtime_planner", "provider": "claude", "model": "claude-opus-5-5", "effort": "high",
 "source": "revise_escalation", "escalated_from": "medium", "escalation_reason": "plan_review_revise"}
```

- `provider`: sessionを起動したprovider（task 1062、goal 73）。値の集合はworkerのrunの`requested_provider` / `actual_provider`と同じ`domain::Provider`（`claude` / `codex`）。worker以外の役割は今は全てClaudeで動く（ADR-t813-2の決定1）ので、`ActorLaunch::default_of`と`RoleModels::launch`は常に`domain::actor_model::ROLE_PROVIDER`（`claude`）を入れ、段上げ（`escalated`）は元のproviderを引き継ぐ。役割ごとにproviderを選ぶ設定は後続のtaskが足す。`provider`の無い過去の`launch`（task 1062より前の記録）は`ActorLaunch::recorded`が`claude`として読む（それより前はClaudeでしか動かなかった）
- `source`: `default`（渡していない。`model` / `effort`はnullで、実際の値はsessionが閉じたときの`session_closed`の`model` / `effort`が持つ）、`dagq.toml`、`revise_escalation`。`escalated_from` / `escalation_reason`は段上げのときだけ
- **jobの開始のevent**: `review_started`・`triage_started`・`plan_review_started`・`goal_review_started`・`observe_started`・`throughput_review_started`の`launch`、生きているrunの復旧のjobは`recovery_requested`の`launch`（生きているrunのjobのpromptのfactsにも、その後の終わったrunのjobのfactsにも含めない）。どれも`provider`を持つ。jobの区間の`session_opened`（`domain::sessions::changes`の`job`）は開始のeventの`launch`を写す（生きているrunの復旧のjobとスループットの見直しのjobは区間を持たない）。終わったrunの復旧のjobは`triage_started`の`launch`（`ActorLaunch::recorded`）で起動する
- **planner**: 開く側（`open_person_planner` / `open_runtime_planner` / `open_draft_planner`）が決め、`dagq plan`の結果の`launch`、workspaceの`--env DAGQ_LAUNCH=<launchのJSON>`、wrapperのargvの`planner-session ... --model <model> --effort <effort>`（渡すときだけ）にする。wrapper（`run_planner_session`）は`planner_command`の後に`select_model`で渡す。pluginのhookが記録するplannerの区間の`session_opened`は`DAGQ_LAUNCH`を`launch`に写す（`SessionHook::launch`。JSONのobjectとして読めなければ写さない）。同じworkspaceで人が`claude`を打ち直したsessionも同じ`launch`を持つが、その起動には引数が無い
- **reviseの配送**: `plan_revise_sent`に`launch`（新しく開いたplannerのもの、生きているplannerへの配送ではnull）、`effort_raised`（effortが実際に上がったか。`xhigh`・`max`のまま、または知らない値ではfalse）、`effort_not_raised`（生きているplannerへの配送で上げなかった理由、それ以外はnull）

計測の層別（`kpi`の`model=` / `effort=`）はtranscriptから読んだ`session_closed`の値を使う（task 579）。`launch`は起動の意図と出どころで、`default`のsessionの実際の値はtranscriptが持つ。

- **goal review**（task 1062）: `goal_review_started`に`launch`と、runtimeが起動前に決めてjobに渡す`session_id`（ADR-0048の決定4。Claude Codeには`--session-id`）と`cwd`（jobを起動したrepositoryのcheckout）を記録する。これで他のjobと同じく区間（`session_opened` / `session_closed`、kind `goal_review`）を持ち、`session_closed`にtranscriptから読んだ実際の`model` / `effort`が入る（[Agent provider lifecycle](../provider-lifecycle.md#claude-sessionの区間)）。それより前の`goal_review_started`には`launch`も`session_id`も無い
- **実際のmodelをjobの終わりのeventに写さない**: 実際の`model` / `effort`は区間の`session_closed`だけが持ち、`review_finished`・`goal_review_finished`などjobの終わりのeventには写さない。`session_closed`はjobの終わりのeventと同じトランザクション・同じ時刻に書かれ、`opened_event_id`で開始のevent（`launch`）と、payloadの`plan_review_id` / `goal_review_id`・runでjobと結べるので、写すと同じ値を2か所に持つだけになる。`stats`の`sessions`（kindごと）と`kpi`の`model=` / `effort=`の層は既に`session_closed`を読む
- **`headless_jobs.provider`**（schema v55、`migrations/0055_headless_job_provider.sql`）: supervisorが起動するheadlessのjob（review・復旧・plan review・goal review）のプロセスの行に、起動したprovider（今は`ROLE_PROVIDER`）を書く。`NOT NULL DEFAULT 'claude'`なので、migrationより前の行と、列を知らない古いバイナリが書く行は`claude`と読める（[Headless job processes](headless-job-processes.md)）

## テスト

- `domain::actor_model`の単体テスト（表が無い役割、表の値と既定、段上げと上限、記録の読み戻し、effortの検査、全ての役割の`launch`の`provider`と`provider`の無い過去の記録を`claude`と読むこと（`every_launch_records_its_provider_and_an_older_one_reads_as_claude`））
- `infrastructure::run_env`の`parses_the_role_tables`
- `domain::sessions`の`a_hook_input_names_its_session_and_the_environment_its_span`（`DAGQ_LAUNCH`の読み取りと`session_opened`の`launch`）
- `tests/it/actor_model.rs`: 表が無いとき復旧のjobとreviewに何も渡さず`default`を記録し、`[roles.review]`・`[roles.recovery]`があれば渡して`dagq.toml`を記録する（区間の`launch`も）
- `tests/it/plan_review.rs`: `a_revise_without_a_live_planner_opens_planners_within_the_limit`（既定のplan reviewと、開き直したplannerの`high`への段上げ）、`a_revise_goes_to_the_live_planner_...`（生きているplannerは上げない）、`role_tables_set_the_plan_review_and_raise_the_revise_planner_from_them`（`[roles.plan_review]`と、`[roles.runtime_planner]`の`high`から`xhigh`）
- `tests/it/runtime_observer.rs`の`the_observer_takes_its_role_table_and_records_what_it_started_with`
- `tests/it/goal_review.rs`の`a_goal_review_records_its_launch_and_session_and_takes_its_role_table`（`goal_review_started`の`launch`・`session_id`・`cwd`、区間の`session_opened` / `session_closed`、`[roles.goal_review]`が起動に効くこと）と、`infrastructure::sessions`の`a_goal_review_span_records_its_launch_and_the_model_of_its_transcript`（transcriptの`model` / `effort`が`session_closed`に入ること、`interrupted`の行の区間を`inferred`で閉じること）
- `tests/it/runtime_stall_recovery.rs`（生きているrunの`recovery_requested`の`launch.provider`）、`tests/it/runtime_throughput_review.rs`（`throughput_review_started`の`launch`）、`infrastructure::headless_jobs`の`a_job_records_its_provider_and_one_without_reads_as_claude`
- `tests/it/lifecycle_plan.rs`: `plan_gives_the_planner_the_model_and_effort_of_its_role`（`[roles.planner]`と、壊れた値でwarningを出して既定で開くこと）、既定の`DAGQ_LAUNCH`、runtimeのplannerの段上げ、wrapperが`select_model`で渡すこと
