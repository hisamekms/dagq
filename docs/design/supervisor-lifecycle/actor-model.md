---
id: design-supervisor-lifecycle-actor-model
type: design
title: "Actor model"
status: current
created: 2026-09-27
scope: runtime
related:
  - adr-t1394-1
  - adr-t1394-2
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-worker-model
  - design-supervisor-lifecycle-plan-review
  - design-supervisor-lifecycle-goal-review
  - design-supervisor-lifecycle-throughput-review
  - design-supervisor-lifecycle-plan-planners
  - design-provider-lifecycle
  - adr-0079
  - adr-t1063-1
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
| `planner` | 人が`dagq plan`で開いたplanner（main checkoutの`dagq.toml`）。廃止（ADR-t1394-1、task 1399）の後は新しいplannerに使わない | （廃止前の`dagq plan`ごと） |

`runtime_planner`は人のplannerの廃止の後、計画の依頼のplannerも含む。`planner`は人が開くplannerの廃止（[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)、task 1399で`dagq plan`は拒む）の後は新しいplannerに使われない。`runtime_planner`の旧keyの`route`は値に関わらず受け付けて無視する（下の[runtimeのplannerの経路](#runtimeのplannerの経路)）。

- **無いとき**: 役割の表が無い（keyの無い表も同じ）ときは今までと同じ起動で、`--model` / `--effort`を渡さない（Claude Codeの既定。今はOpus 5.5・medium）。この task を入れただけでは挙動は変わらない
- **あるとき**: 表の`model`と`effort`を`AgentProvider::apply_launch`が`AgentProvider::select_model`（task 576の仕組み。Claude Codeでは`--model <model> --effort <effort>`をoptionの末尾、`--`の前）で渡す。片方だけ書いたときは、もう片方は既定（`claude-opus-5-5` / `medium`）を明示して渡す
- **読めないとき**: supervisorとobserverはwarnをlogに出して今までと同じ起動にする（壊れた`dagq.toml`はprovisioningがerrorにする）。（廃止前の`dagq plan`は結果の`warnings`に足して今までと同じ起動でplannerを開いた）
- 設定は読む時点ごとに読み直すので、supervisorの再起動なしに次のjobから効く
- この repositoryの`dagq.toml`の役割の表は`[roles.goal_review]`（task 1067、goal 73）と`[roles.review]`（task 1208、goal 80）と`[roles.plan_review]`（task 1219、goal 80）と`[roles.throughput_review]`（task 1221、goal 80）と`[roles.observer]`（task 1224、goal 80）の`provider = "codex"`と、`[roles.runtime_planner]`の`route = "headless"`（task 1402、goal 87。固定バイナリがtask 1396・1397・1398を含むことをverifyの関門が確かめてから着地させた。評価は[headless-planner-evaluation](../../plans/headless-planner-evaluation.md)。task 1441からは受け付けて無視する旧keyで、下の[runtimeのplannerの経路](#runtimeのplannerの経路)）だけで（下の[provider](#provider)）、どの役割にも`model` / `effort`は置かない（highに上げるのは基準値がたまってから人とplannerが決める。ADR-0079の決定7の(d)）。旧バイナリは`[roles.*]`を未知の表として拒み、`provider`を知らないバイナリも、その役割を`CODEX_ROLES`に持たないバイナリの`codex`も拒むので、足すのはそれを知るバイナリ（goal_reviewはtask 1065、reviewはtask 1207、plan_reviewはtask 1218、throughput_reviewはtask 1220、observerはtask 1223とtask 1378）に固定バイナリが入れ替わった後にした（plan_reviewはtask 1219の、throughput_reviewはtask 1221のverifyの関門が、固定バイナリのbuild識別子のcommitがそれぞれtask 1218・task 1220の着地commitを含むことを確かめてから着地させた。observerはtask 1224のverifyの関門で、固定バイナリがtask 1223と、Gitの外のjobに`--skip-git-repo-check`を付けるtask 1378の両方の着地commitを含むことを確かめる）

## provider

[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)の決定1・4・5（task 1065）。`[roles.<role>]`の`provider`（`claude` / `codex`）で役割のsessionを動かすproviderを選ぶ。

```toml
[roles.goal_review]
provider = "codex"
model = "gpt-6-astra"   # 省けばCodexの既定のmodel
effort = "high"         # 省けばmedium
```

- **既定**: `provider`を書かない役割はClaude（`domain::actor_model::ROLE_PROVIDER`）で、起動も失敗のときの振る舞い（Claudeの控えのaskで待つ）も今までと同じ
- **Codexで動ける役割**: `domain::actor_model::CODEX_ROLES`（`goal_review`・`review`・`plan_review`（task 1218）・`throughput_review`（task 1220）・`observer`（task 1223、[ADR-t1222-1](../../adr/2026-10-02-t1222-1-codex-observer-writes-through-the-queue-service.md)）・`recovery`（終わったrunと生きているrunの復旧job、task 1225）。`runs_on(role, provider)`）。それ以外の役割（`runtime_planner`・`planner`）に`codex`を書くと、`dagq.toml`を読むときの検査（`RoleModels::check`、`parse_config`の最後）がエラーにする（`dagq.toml: [roles.runtime_planner]: provider codex cannot run the runtime_planner role; Codex runs only goal_review, review, plan_review, throughput_review, observer, recovery`）。warnを出してClaudeで起動する形にしなかったのは、ADR-t1063-1の決定1が「起動せずに設定の誤りとして知らせる」と決めたため。エラーの間は他の`dagq.toml`の誤りと同じく、supervisorとobserverは全ての役割を今までと同じ起動にし（`Supervisor::role_models`のwarn）、`doctor`の`roles.error`に出る。`codex`の表にClaudeのmodel（`claude`で始まる名前）を書いてもエラーにする（modelはproviderごとの名前で、CodexにClaudeのmodelを渡さない）
- **launch**（`RoleModels::launch`）: Claudeの表は今までどおり（省いたmodel / effortは`claude-opus-5-5` / `medium`を明示）。Codexの表はmodelを省けば`null`（Codexの既定。`-m`を渡さない）、effortを省けば`medium`（`-c model_reasoning_effort`に渡す）。どちらも`source`は`dagq.toml`
- **切り替え**（`RoleModels::switchable`・`domain::actor_model::job_route`。起動の決まったjobの行き先の値は同じmoduleの`JobStartRoute`（`Start` / `Unavailable`）で、同じmoduleの`job_start_route`が決める）: `provider`を書いた役割のjobは、そのproviderが使えない（supervisorにagentが無い＝`executable_missing`、または控えられている）とき、もう一方のproviderがその役割を動かせて使えれば、そのproviderの既定の起動（modelとeffortはproviderごとの名前なので渡さない。`ActorLaunch::switched`）で起動し、launchに`switched_from`（元のprovider）と`switch_reason`（`SwitchReason`の値）を残す。どちらも使えなければ起動せずに待つ。`provider`を書いたこと（`switchable`）は失敗を使えないことに分類してそのproviderを控えることを決め、もう一方へ移ってよいかは別の引数の`[provider_fallback] jobs`（[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)、task 1858）が決める: `jobs = false`なら`job_route`は使えない理由（`SwitchReason::unusable`）ではもう一方へ移さず`JobRoute::Wait`を返し、控えが解けた後に同じproviderで起動する（`--no-claude`の`Disabled`と、runのreviewの`subagents_unsupported`による能力の選択はoffでも移る。[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)の「切り替えを止める設定」）。今これを使うのはgoal review（[Goal review](goal-review.md)の3と6）とrunのreview（`Supervisor::review_route`。[Review](review.md)、[ADR-t1207-1](../../adr/2026-09-30-t1207-1-codex-run-review.md)。`--no-claude`で切り替え先が無ければ待たずに手動reviewに渡す）とplan review（`Supervisor::plan_review_route`、task 1218。[Plan review](plan-review.md#codexで動かす)。`--no-claude`で切り替え先が無ければ待たずに`plan_review_failed`でplan review by handに渡す）とスループットの見直し（`Supervisor::job_start_route`、task 1220。[Throughput review](throughput-review.md#codexで動かす)。`--no-claude`で切り替え先が無ければ、見直しの要る期間の`throughput_review_finished`（`outcome: error`）に理由を残す）とobserver（同じ`Supervisor::job_start_route`、task 1223。[Observer](observer.md#codexで動かす)。`--no-claude`で切り替え先が無ければ、skipされないobservationの`observe_finished`（`outcome: error`、`unavailable: true`）に理由を残す）と復旧job（`Supervisor::recovery_route`、task 1225。[Triage](triage.md#codexで動かす)・[生きているsessionの復旧job](background-recovery-job.md#codexで動かす)。jobの途中でproviderが使えなくなったときも、終わったrunは`provider_unusable`の`triage_failed`の後の次のラウンド、生きているrunはalertの次のjobをもう一方で（`jobs = false`なら控えが解けた後に同じproviderで）始める。`--no-claude`で切り替え先が無ければjobを起動せず、終わったrunは`triage_failed`、生きているrunは`recovery_failed`のaskに理由を残す）で、他の役割は`provider = "claude"`を書いても今までどおりの起動と控え
- **見え方**: `dagq doctor`の`roles`が役割ごとに`provider`・`source`（`dagq.toml` / `default`）・`model`・`effort`を出す（queueが束縛されたmain checkoutの`dagq.toml`。読めなければ`roles.error`に理由を足し、全ての役割は既定で出る。`compose::doctor_roles`）。jobごとに実際に動いたproviderは開始のeventの`launch.provider`と`headless_jobs.provider`

## 差し戻しで開き直すplannerの段上げ

plan reviewの`revise`（と`reopen`）の指摘を配るとき、持ち主のplannerが居ないのでruntimeが新しくplannerを立てる（`open_runtime_planner`、[Plan review](plan-review.md)の9）と、そのplannerのeffortを`[roles.runtime_planner]`の値（無ければ`medium`）から1段上げる（`domain::actor_model::raise`: `low` → `medium` → `high` → `xhigh`、`xhigh`が上限で同じ段のまま。`max`は上げない）。modelは表の値（無ければ`claude-opus-5-5`を明示）のまま変えない。理由（`escalation_reason`）は`plan_review_revise`。

生きているplannerへの配送（指摘を次のturnの依頼として`turns/`に置く）は新しく起動しないので上げない。runtimeは動いているsessionの中でmodel / effortを切り替えないので、`plan_revise_sent`に上げなかった理由（`effort_not_raised`）を残す。

## 記録（`launch`）

起動したprovider・model / effortと、model / effortの出どころを`launch`（`domain::actor_model::ActorLaunch`）として記録する。

```json
{"role": "runtime_planner", "provider": "claude", "model": "claude-opus-5-5", "effort": "high",
 "source": "revise_escalation", "escalated_from": "medium", "escalation_reason": "plan_review_revise"}
```

- `provider`: sessionを起動したprovider（task 1062、goal 73）。値の集合はworkerのrunの`requested_provider` / `actual_provider`と同じ`domain::Provider`（`claude` / `codex`）。`ActorLaunch::default_of`は`domain::actor_model::ROLE_PROVIDER`（`claude`）を、`RoleModels::launch`は表の`provider`（無ければ`claude`。上の[provider](#provider)、task 1065）を入れ、段上げ（`escalated`）は元のproviderを引き継ぐ。`provider`の無い過去の`launch`（task 1062より前の記録）は`ActorLaunch::recorded`が`claude`として読む（それより前はClaudeでしか動かなかった）
- `switched_from` / `switch_reason`: 使えないproviderから切り替えて起動したjobだけ（task 1065）。必須のreviewのsubagentを動かせないproviderからrunのreviewを切り替えたときは`switch_reason: subagents_unsupported`（providerは使えるので控えにしない。[Review](review.md#reviewのsubagent)、task 1455）。例: `{"role": "goal_review", "provider": "claude", "model": null, "effort": null, "source": "default", "switched_from": "codex", "switch_reason": "authentication"}`
- `source`: `default`（渡していない。`model` / `effort`はnullで、実際の値はsessionが閉じたときの`session_closed`の`model` / `effort`が持つ）、`dagq.toml`、`revise_escalation`。`escalated_from` / `escalation_reason`は段上げのときだけ
- **jobの開始のevent**: `review_started`・`triage_started`・`plan_review_started`・`goal_review_started`・`observe_started`・`throughput_review_started`の`launch`、生きているrunの復旧のjobは`recovery_requested`の`launch`（生きているrunのjobのpromptのfactsにも、その後の終わったrunのjobのfactsにも含めない）。どれも`provider`を持つ。jobの区間の`session_opened`（`domain::sessions::changes`の`job`）は開始のeventの`launch`を写す（生きているrunの復旧のjobは区間を持たない）。終わったrunの復旧のjobは`triage_started`の`launch`（`ActorLaunch::recorded`）で起動する
- **planner**: 開く側（`open_runtime_planner` / `open_draft_planner`）が決め、開いた結果（`OpenedPlanner`）の`launch`、backgroundで立てるwrapperの環境の`DAGQ_LAUNCH=<launchのJSON>`（`actor_env`）、wrapperのargvの`planner-session ... --model <model> --effort <effort>`（渡すときだけ）にする。wrapper（`run_planner_session`）はturnごとのcommandに`select_model`で渡し、turnの`turn_started`に`launch`を書く（下の[runtimeのplannerの経路](#runtimeのplannerの経路)）。廃止前に人が開いたplanner（端末の`planner_command`の後に`select_model`）では、pluginのhookが記録するplannerの区間の`session_opened`が`DAGQ_LAUNCH`を`launch`に写す（`SessionHook::launch`。JSONのobjectとして読めなければ写さない）
- **reviseの配送**: `plan_revise_sent`に`launch`（新しく開いたplannerのもの、生きているplannerへの配送ではnull）、`effort_raised`（effortが実際に上がったか。`xhigh`・`max`のまま、または知らない値ではfalse）、`effort_not_raised`（生きているplannerへの配送で上げなかった理由、それ以外はnull）

計測の層別（`kpi`の`model=` / `effort=`）はtranscriptから読んだ`session_closed`の値を使う（task 579）。`launch`は起動の意図と出どころで、`default`のsessionの実際の値はtranscriptが持つ。

- **goal review**（task 1062）: `goal_review_started`に`launch`と、runtimeが起動前に決めてjobに渡す`session_id`（ADR-0048の決定4。Claude Codeには`--session-id`）と`cwd`（jobを起動したrepositoryのcheckout）を記録する。これで他のjobと同じく区間（`session_opened` / `session_closed`、kind `goal_review`）を持ち、`session_closed`にtranscriptから読んだ実際の`model` / `effort`が入る（[Agent provider lifecycle](../provider-lifecycle.md#claude-sessionの区間)）。それより前の`goal_review_started`には`launch`も`session_id`も無い
- **スループットの見直し**（task 1086）: `throughput_review_started`の`session_id`・`launch`・`dir`で、observerと同じqueueの区間（kind `throughput_review`）を開き、同じ`session_id`の`throughput_review_finished`で閉じる。`session_closed`には他のjobと同じ経路（task 579）でtranscriptから読んだ実際の`model` / `effort`が入る。見直しは同時に複数走りうる（execの引き継ぎで前のprocessが残したjob、別のsupervisorのjob）ので、始まりは同じ`mode`と`period`の区間だけを、終わりは自分の区間だけを閉じ、終わりの無いまま35分（`RUNNING_MS`）を過ぎた区間は次の見直しのeventで`inferred`として閉じる（規則は[スループットの見直し](throughput-review.md#sessionの区間task-1086)）
- **実際のmodelをjobの終わりのeventに写さない**: 実際の`model` / `effort`は区間の`session_closed`だけが持ち、`review_finished`・`goal_review_finished`などjobの終わりのeventには写さない。`session_closed`はjobの終わりのeventと同じトランザクション・同じ時刻に書かれ、`opened_event_id`で開始のevent（`launch`）と、payloadの`plan_review_id` / `goal_review_id`・runでjobと結べるので、写すと同じ値を2か所に持つだけになる。`stats`の`sessions`（kindごと）と`kpi`の`model=` / `effort=`の層は既に`session_closed`を読む
- **Codexのjobの実際のmodelとthread**（task 1065）: Codexはsessionのidをrunを始める前に受け取らず（threadを自分で名付ける）、transcriptも無いので、Codexのgoal reviewは`goal_review_started`の`session_id`をnullにし（plan reviewも同じく`plan_review_started`の`session_id`をnullにし、`plan_review_finished` / `plan_review_failed`に同じ値を写す。task 1218。スループットの見直しも`throughput_review_started`の`session_id`をnullにし、`throughput_review_finished`に同じ値を写す。task 1220。runのreviewも`review_started`の`session_id`をnullにし、`review_finished`（pass / revise / concern）・`review_failed`・`review_retried`に同じ値を写す。task 1339。observerも`observe_started`の`session_id`をnullにし、`observe_finished`に同じ値を写す。task 1223。終わったrunの復旧jobも`triage_started`の`session_id`をnullにし、`triage_finished` / `triage_failed`と`recovery_finished`に、生きているrunの復旧jobは`recovery_finished`に同じ値を写す。task 1225）、終わりのevent（`goal_review_finished` / `goal_review_failed`）に`session_id`（threadのid）・`model`（rolloutから読んだ実際のmodel）・`model_unknown`（読めなかった理由。読めたときは無い）を写す（`domain::headless_job::JobSession::record`）。区間の`session_closed`も同じ値を持つ（[Agent provider lifecycle](../provider-lifecycle.md#claude-sessionの区間)）。上の「写さない」はClaudeのjobの話で、Codexのjobは終わりのeventしか値の出どころが無いので写す。`stats`のjobの`by_model`は、開始の`session_id`の`session_closed`が無ければ終わりのeventの`model`を読む
- **`headless_jobs.provider`**（schema v55、`migrations/0055_headless_job_provider.sql`）: supervisorが起動するheadlessのjob（review・復旧・plan review・goal review）のプロセスの行に、起動したprovider（`JobSubject::provider`。goal review・runのreview（ADR-t1207-1）・plan review（task 1218）・復旧（task 1225）は行き先で実際に起動したprovider（launchの`provider`。切り替えた後ならその先））を書く。`NOT NULL DEFAULT 'claude'`なので、migrationより前の行と、列を知らない古いバイナリが書く行は`claude`と読める（[Headless job processes](headless-job-processes.md)）

## runtimeのplannerの経路<a id="runtimeのplannerの経路"></a>

経路（対話か非対話）の選択は[ADR-t1394-2](../../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)の決定1（goal 87、task 1396）で入れた。

[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定3（goal 92、task 1441）で対話の経路を廃止し、runtimeのplannerは経路の選択なしに非対話だけで動く（起動とsessionは[非対話のruntimeのplanner](headless-worker.md#非対話のruntimeのplanner)）。

```toml
[roles.runtime_planner]
route = "headless"   # 旧key。値に関わらず受け付けて無視する
```

- 旧keyの`route`は`[roles.runtime_planner]`でだけ、値に関わらず（`interactive`・`headless`・知らない値も）受け付けて無視する（`infrastructure::run_env::parse_config`、`domain::actor_model::ROUTE_KEY`）。書いてある`dagq.toml`もそのまま読める（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)決定2の切り替えの欄と同じ扱いで、後で`dagq.toml`から消す。supervisorはplannerを開くとき（`planner_launch`が`[roles]`を読むたび）に、keyがあれば値に関わらず（`headless`も）、無視することを`warn`で1回だけlogに書く（`warn_ignored_route_setting`。`[roles.runtime_planner] route = "<値>" of dagq.toml is ignored: the runtime's planners run headless only, in the background (ADR-t1433-2)`。値は`RoleModels::ignored_planner_route`に残す）。経路は`dagq doctor`の`roles.runtime_planner.route`が`headless`だけを示す）。他の役割の表に書けば「route is a key of [roles.runtime_planner] only (and ignored there)」でエラーにし、重複は他のkeyと同じく「is defined twice」。`RoleModel`は`route`を持たず、`RoleModels::planner_route`は無い。`provider`は`claude`のまま（Codexのplannerは別のgoal。`CODEX_ROLES`に`runtime_planner`は入らない）。
- `route`だけを書いた表は起動に何も渡さない（`RoleModels::launch`は`provider` / `model` / `effort`のどれも無い表を表の無い役割と同じに扱い、`default`の起動のまま）。
- supervisorはplannerを開くたびに`[roles]`を読み（`PlannerLaunch::roles`。model / effortのため）、開くruntimeのplannerの`planners.route`には常に`headless`を書く（[`plan` / `planners`](plan-planners.md#runtimeのplannerの経路)）。`planners.route`は、過去の対話のplanner（古いバイナリが開いたruntimeのplannerと、廃止前に人が`dagq plan`で開いたplanner）の行を`interactive`のまま読むために残る（`stats` / `kpi`の経路ごとの集計）。
- `dagq doctor`の`roles.runtime_planner`は`route: "headless"`だけを示し、`route_source`は無い（`compose::doctor_roles`）。読めない`dagq.toml`では他の役割と同じく`error`を出す。
- reviseで開き直すplannerの段上げは上の[差し戻しで開き直すplannerの段上げ](#差し戻しで開き直すplannerの段上げ)のとおり。生きているplannerへの配送は新しく起動しないので上げない（`effort_not_raised`）。

非対話のplannerの区間はturnから開いて閉じ、`route: headless`を持ち、`stats` / `kpi`で経路ごとに読む（ADR-t1394-2決定4、task 1398。[`plan` / `planners`](plan-planners.md#runtimeのplannerの経路)、[provider-lifecycle](../provider-lifecycle.md#claude-sessionの区間)）。wrapperはplannerのturnの`turn_started`に起動の`model`と`effort`を`launch`として書き、区間はそれを持つ。

まだ実装していないもの（goal 87の後続task）: `launch`への`route`の記録（経路は区間の`route`とplannerの行の`route`にある）、`[roles.planner]`が人のplannerの廃止の後に読まれないことの`doctor`への表示。

## テスト

- `domain::actor_model`の単体テスト（表が無い役割、表の値と既定、段上げと上限、記録の読み戻し、effortの検査、全ての役割の`launch`の`provider`と`provider`の無い過去の記録を`claude`と読むこと（`every_launch_records_its_provider_and_an_older_one_reads_as_claude`））
- `infrastructure::run_env`の`the_old_route_of_the_runtimes_planners_is_accepted_and_ignored`（`[roles.runtime_planner]`の`route`はどの値でも読めて起動に何も渡さず、警告のために値を`RoleModels::ignored_planner_route`に残し、他のkeyはそのまま効くこと、`[roles.runtime_planner]`の外の`route`と重複のエラー）
- `tests/it/planner_headless.rs`の`the_old_route_key_is_ignored_and_the_runtimes_planner_runs_headless`: 代表の1つ`route = "interactive"`の`dagq.toml`で（値ごとの読み方と残す値はunit testの`infrastructure::run_env`の`the_old_route_of_the_runtimes_planners_is_accepted_and_ignored`、警告を1回にする判定は`application::supervise::plan_review`の`the_old_route_key_is_warned_of_whatever_its_value_and_only_once`が持つ。`headless_fixture`も`route = "interactive"`を書く）、supervisorが無視することの警告をlogに1回だけ書き、reviseに開いたplannerが`headless`でbackgroundのwrapperで立ち、次のreviseがturnの依頼になり、`doctor`の`roles.runtime_planner`が`route: headless`を示して`route_source`を持たないこと
- `infrastructure::run_env`の`parses_the_role_tables`（`provider`の値、`CODEX_ROLES`以外（`runtime_planner`）の`codex`とCodexの表のClaudeのmodelのエラー、`plan_review`・`throughput_review`・`observer`の`codex`を受け付けること）
- `domain::actor_model`の`a_role_table_names_its_provider`と`a_job_moves_to_the_other_provider_or_waits`（task 1065）
- `tests/it/goal_review_codex.rs`（task 1065）: `provider = "codex"`でgoal reviewがstubの`codex exec --json`（`:read-only`を継ぐjobのpermission profile `dagq_job`。goal 82の段(3)）で動き、jobの`dagq`がqueue service経由で読み、verdict・thread・modelを読んで適用し、`doctor`の`roles`とgoal review以外の`codex`のエラー、Codexの認証の失敗からClaudeへの切り替え、両方控えられたときの待ち。providerの状態ごとの行き先（Codexが無いときのClaude、Claudeの控えのaskが開いたままのCodex、`--no-claude`）は`application::supervise::goal_review`のunit test `the_goal_review_starts_on_a_provider_it_can_use_or_waits`
- `tests/it/plan_review_codex.rs`（task 1218）: `provider = "codex"`でplan reviewがstubの`codex exec --json`（goal reviewと同じjobのpermission profile `dagq_job`）で動き、jobの`dagq`がqueue service経由で読み、pass・revise・concernをClaudeのjobと同じ経路で適用し、開始の`launch.provider`・終わりのthreadとmodel・`headless_jobs.provider`・`stats`と`kpi`のproviderごとの集計、Codexの認証の失敗からClaudeへの切り替え、`--no-claude`でCodexで動きClaudeに戻らず、Codexが使えなければ理由付きでplan review by handに渡ること
- `tests/it/throughput_review_codex.rs`（task 1220）: `provider = "codex"`で時間・日次・週次の見直しがstubの`codex exec --json`（jobのpermission profile `dagq_job`）で動き、jobの`dagq`がqueue service経由で読み、最後の返答をClaudeの出力と同じく`reports/reviews/`への保存・週次の次の一手のfinding・`throughput_review_reported`にし、開始の`launch.provider`・終わりと区間のthreadとmodel・`stats`と`kpi`のproviderごとの集計、利用上限で止まったCodexの`provider_unusable`、`--no-claude`でCodexで動き、失敗してもClaudeを起動せず理由を記録すること。`domain::stats::jobs`の`a_codex_throughput_review_is_counted_under_codex_and_its_model`
- `tests/it/observer_codex.rs`（task 1223）: `provider = "codex"`で毎時と日次のobservationがstubの`codex exec --json`（jobのpermission profile `dagq_job`。書けるrootもsandboxを外すflagも無い）で動き、jobの`dagq`がクライアントモードでqueue service経由でfindingの記録・更新・resolveとfindingに紐づく`blocked`のaskを書き（帰属は`observer`、noteはserviceが拒む）、`observe_finished`の件数とid、開始の`launch.provider`・終わりと区間のthreadとmodel（rolloutが無ければ`model_unknown`）・`stats`と`kpi`のproviderごとの集計、利用上限で止まったCodexの`provider_unusable`、`--no-claude`でCodexで動き、失敗してもClaudeを起動せず理由を記録すること、Claudeが使えればClaudeで始め直すこと、表が無ければ今までどおりClaudeで動くこと。`domain::stats::jobs`の`a_codex_observation_is_counted_under_codex_and_its_model`
- `tests/it/recovery_codex.rs`（task 1225）: `provider = "codex"`で終わったrunと生きている（stalledの）runの復旧jobがstubの`codex exec --json`の読み取りだけのsandbox（書けるrootもsandboxを外すflagも無い）で動き、verdictがClaudeのjobと同じ検査（許す操作・`confidence`・前提・回数）を通って適用（`retry`・`resume`・`wait`）かask（終わったrunの取らない操作・`confidence: low`）になること、`triage_started` / `recovery_requested`の`launch.provider`・終わりと区間のthreadとmodel・`headless_jobs.provider`・`stats`と`kpi`のproviderごとの集計、失敗と時間の上限が`triage_failed` / `recovery_failed`のaskになり時間の上限でjobの子孫が止まること、ログインで失敗したCodexのjobが控えられて人に渡らず、終わったrunの次のラウンドと生きているrunの次のjobがClaudeで動くこと、`--no-claude`でCodexで動きClaudeを起動せず、ログインで失敗した後は理由付きで人に渡ること。行き先と終わりの記録は`application::supervise::recovery`のunit test
- `domain::sessions`の`a_hook_input_names_its_session_and_the_environment_its_span`（`DAGQ_LAUNCH`の読み取りと`session_opened`の`launch`）
- `tests/it/actor_model.rs`: 表が無いとき復旧のjobとreviewに何も渡さず`default`を記録し、`[roles.review]`・`[roles.recovery]`があれば渡して`dagq.toml`を記録する（区間の`launch`も）
- `tests/it/plan_review.rs`: `a_revise_without_a_live_planner_opens_planners_within_the_limit`（既定のplan reviewと、開き直したplannerの`high`への段上げ）、`a_revise_goes_to_the_live_planner_...`（生きているplannerは上げない）、`tests/it/plan_review_turns.rs`の`role_tables_set_the_plan_review_and_raise_the_revise_planner_from_them`（`[roles.plan_review]`と、`[roles.runtime_planner]`の`high`から`xhigh`）
- `tests/it/runtime_observer.rs`の`the_observer_takes_its_role_table_and_records_what_it_started_with`
- `infrastructure::sessions`の`a_throughput_review_span_records_its_launch_and_the_model_of_its_transcript`（スループットの見直しの区間の`launch`、`session_closed`のtranscriptの`model` / `effort`、並ぶ見直しと時間を過ぎた区間の閉じ方、observerの区間が変わらないこと。task 1086）
- `tests/it/goal_review.rs`の`a_goal_review_records_its_launch_and_session`（`goal_review_started`の`launch`・`session_id`・`cwd`、区間の`session_opened` / `session_closed`）と`application::supervise::goal_review`の`a_role_table_gives_the_goal_review_its_model_and_effort`（`[roles.goal_review]`のmodel・effortから起動の`launch`への対応。committedの表を読みjobに渡す配線はroleに共通で、Claudeは`tests/it/plan_review_turns.rs`の`role_tables_set_the_plan_review_and_raise_the_revise_planner_from_them`、goal reviewのCodexは`tests/it/goal_review_codex.rs`の`a_goal_review_on_codex_runs_read_only_and_records_its_thread_and_model`）と、`infrastructure::sessions`の`a_goal_review_span_records_its_launch_and_the_model_of_its_transcript`（transcriptの`model` / `effort`が`session_closed`に入ること、`interrupted`の行の区間を`inferred`で閉じること）
- `tests/it/recovery_codex.rs`の`a_codex_recovery_job_of_a_stalled_run_resumes_it_and_it_lands`（生きているrunの`recovery_requested`の`launch.provider`。task 1225。以前これを持っていた`tests/it/runtime_stall_recovery.rs`は対話のrunとともに消えた）、`tests/it/runtime_throughput_review.rs`（`throughput_review_started`の`launch`）、`infrastructure::headless_jobs`の`a_job_records_its_provider_and_one_without_reads_as_claude`
- `tests/it/lifecycle_plan.rs`: runtimeのplannerのbackgroundのwrapperのargvの`--model` / `--effort`と環境（`each_runtime_planner_starts_its_wrapper_in_the_background_without_a_workspace`）、runtimeのplannerの段上げと`DAGQ_LAUNCH`（`the_runtime_opens_a_planner_for_a_proposal_with_its_reasons`）、wrapperが`select_model`で渡すこと（`[roles.planner]`を読んだ`dagq plan`のtestは、`dagq plan`の廃止とともに消した）
