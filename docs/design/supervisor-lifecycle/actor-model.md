---
id: design-supervisor-lifecycle-actor-model
type: design
title: "Actor model"
status: current
created: 2026-09-27
scope: runtime
related:
  - adr-t1181-1
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

worker以外のアクターのsessionのmodelとeffortの設定と、provider・model・effortの記録（[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)の決定7の(b)(c)）。
workerは[Worker model](worker-model.md)。
計測（transcriptから読む実際のmodel / effort）は[Agent provider lifecycle](../provider-lifecycle.md#modelとeffort)。

## 役割と`[roles.<role>]`

`dagq.toml`の`[roles.<role>]`で役割ごとに`model`（空でない文字列）と`effort`（`domain::actor_model::EFFORTS`の値）を置ける（書式は[Run environment](run-environment.md)、型は`domain::actor_model::RoleModels`）。

| role | sessionと起動する場所 | 読む時点 |
|---|---|---|
| `plan_review` | plan reviewのjob（`PlanningState::start_plan_review`） | jobの開始ごと |
| `review` | runのreviewのjob（`begin_review`。agentの`provider`は[Review](review.md#agentのjobのprovider)） | jobの開始ごと |
| `recovery` | 復旧（triage）のjob。終わったrunの`begin_triage`と、生きているrunの復旧のjob | jobの開始ごと |
| `goal_review` | goal reviewのjob（`PlanningState::start_goal_review`） | jobの開始ごと |
| `observer` | observerのjob（`dagq observe`。queueが束縛されたcheckoutの`dagq.toml`） | 観測ごと |
| `throughput_review` | スループットの見直しのjob（`dagq throughput-review`。queueが束縛されたcheckoutの`dagq.toml`） | 見直しごと |
| `setup_review` | `dagq setup-review`の生成のjob（main checkoutの`dagq.toml`。[setup-review](setup-review.md)） | 生成ごと |
| `runtime_planner` | runtimeが立てるplanner（reviseのplanner、draftのplanner、findingのplanner、計画の依頼のplanner） | plannerを開くpassごと |
| `planner` | 人が`dagq plan`で開くplanner（main checkoutの`dagq.toml`） | —（下） |

`planner`は、人が開くplannerを[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)が廃止し`dagq plan`は拒むので、新しいplannerに使われない。

- **無いとき**: 役割の表が無い（keyの無い表も同じ）ときはmodel / effortを渡さない（Claude Codeの既定）
- **あるとき**: 表の`model`と`effort`を`AgentProvider::apply_launch`が渡す。
  片方だけ書いたときは、もう片方は既定（`RoleModels::launch`）を明示して渡す
- **読めないとき**: supervisorとobserverはwarnをlogに出して、役割の表が無いときと同じ起動にする（壊れた`dagq.toml`はprovisioningがerrorにする）
- 設定は読む時点ごとに読み直すので、supervisorの再起動なしに次のjobから効く

## provider

[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)の決定1・4・5。
`[roles.<role>]`の`provider`（`claude` / `codex`）で役割のsessionを動かすproviderを選ぶ。

```toml
[roles.goal_review]
provider = "codex"
model = "gpt-6-astra"   # 省けばCodexの既定のmodel
effort = "high"         # 省けば既定
```

- **既定**: `provider`を書かない役割はClaude（`domain::actor_model::ROLE_PROVIDER`）で、失敗のときはClaudeの控えのaskで待つ
- **Codexで動ける役割**: `domain::actor_model::CODEX_ROLES`（observerは[ADR-t1222-1](../../adr/2026-10-02-t1222-1-codex-observer-writes-through-the-queue-service.md)）。
  それ以外の役割に`codex`を書くと、`dagq.toml`を読むときの検査（`RoleModels::check`）がエラーにする。
  warnを出してClaudeで起動する形にしなかったのは、ADR-t1063-1の決定1が「起動せずに設定の誤りとして知らせる」と決めたため。
  エラーの間は他の`dagq.toml`の誤りと同じく、supervisorとobserverは全ての役割を表が無いときと同じ起動にし（`Supervisor::role_models`のwarn）、`doctor`の`roles.error`に出る。
  `codex`の表にClaudeのmodel（`claude`で始まる名前）を書いてもエラーにする（modelはproviderごとの名前で、CodexにClaudeのmodelを渡さない）
- **launch**（`RoleModels::launch`）: Claudeの表は上の「無いとき」「あるとき」のとおり（省いたmodel / effortは既定を明示）。
  Codexの表も省いたmodel / effortは既定（値は`RoleModels::launch`のdoc comment）。
  どちらも`source`は`dagq.toml`
- **切り替え**（`domain::actor_model::job_route`）: `provider`を書いた役割のjobは、そのproviderが使えない（supervisorにagentが無い＝`executable_missing`、または控えられている）とき、もう一方のproviderがその役割を動かせて使えれば、そのproviderの既定の起動（modelとeffortはproviderごとの名前なので渡さない。`ActorLaunch::switched`）で起動し、launchに切り替え元のproviderと理由を残す（`ActorLaunch`）。
  どちらも使えなければ起動せずに待つ。
  `provider`を書いたこと（`switchable`）は失敗を使えないことに分類してそのproviderを控えることを決め、もう一方へ移ってよいかは別の引数の`[provider_fallback] jobs`（[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）が決める: `jobs = false`なら`job_route`は使えない理由（`SwitchReason::unusable`）ではもう一方へ移さず`JobRoute::Wait`を返し、控えが解けた後に同じproviderで起動する（Claudeを使わない運転（ADR-t1204-1）の`Disabled`だけはoffでも移る。[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)の「切り替えを止める設定」）。
  これを使うjobと、Claudeを使わない運転で切り替え先が無いときのjobごとの振る舞いは各jobの文書が持つ（[Goal review](goal-review.md)の3と6、[Review](review.md)（[ADR-t1207-1](../../adr/2026-09-30-t1207-1-codex-run-review.md)）、[Plan review](plan-review.md#codexで動かす)、[Throughput review](throughput-review.md#codexで動かす)、[Observer](observer.md#codexで動かす)、[Triage](triage.md#codexで動かす)・[生きているsessionの復旧job](background-recovery-job.md#codexで動かす)）。
  他の役割は`provider = "claude"`を書いても、`provider`を書かない役割と同じ起動と控え。
  `setup_review`は`job_route`を通らず、`provider`のproviderが使えなくても切り替えず待たず、起動の失敗や0でない終了をそのまま人かinboxのコマンドの誤りにする（[setup-review](setup-review.md#権限)）
- **inbox**: 今は役割の表に無く、`dagq inbox`はClaudeで開く（[`up` / `down`](up-down.md)の「`dagq inbox`」）。
  [ADR-t1181-1](../../adr/2026-10-24-t1181-1-codex-inbox-provider-selection-gaps-unavailability-and-records.md)決定1・4はinboxを役割に足し（起動の引数でその起動だけ上書き）、`job_route`の切り替えを当てず、providerの控えでinboxを止めないと決めた（実装前）
- **見え方**: `dagq doctor`の`roles`が役割ごとの起動のproviderとmodel / effortと出どころを出す（queueが束縛されたmain checkoutの`dagq.toml`。`application::health::roles`）。
  jobごとに実際に動いたproviderは開始のeventの`launch.provider`と`headless_jobs.provider`

## 差し戻しで開き直すplannerの段上げ

plan reviewの`revise`（と`reopen`）の指摘を配るとき、持ち主のplannerが居ないのでruntimeが新しくplannerを立てる（`open_runtime_planner`、[Plan review](plan-review.md#reviseの配送)）と、そのplannerのeffortを`[roles.runtime_planner]`の値（無ければ既定）から1段上げる（`domain::actor_model::raise`）。
modelは表の値（無ければ既定を明示）のまま変えない。

生きているplannerへの配送（指摘を次のturnの依頼として`turns/`に置く）は上げず、理由と記録は`infrastructure::plan_reviews`の`revise_sent`のdoc commentが持つ。

## 記録（`launch`）

起動したprovider・model / effortと、model / effortの出どころを`launch`（`domain::actor_model::ActorLaunch`）として記録する。

- runのreviewでは`launch`をjobごとに記録する（`review_job_started`の`launch`。[Review](review.md#reviewのprovider)）。
- **jobの開始のevent**: 各jobの開始のevent（生きているrunの復旧のjobは`recovery_requested`）が`launch`を持つ（生きているrunのjobのpromptのfactsにも、その後の終わったrunのjobのfactsにも含めない）。
  jobの区間の`session_opened`（`domain::sessions`の`job`）は開始のeventの`launch`を写す（生きているrunの復旧のjobは区間を持たない）。
  終わったrunの復旧のjobは`triage_started`の`launch`（`ActorLaunch::recorded`）で起動する
- **planner**: 開く側（`application::planner::launch_planner`）が決めてbackgroundのwrapperに渡し、wrapperはturnの`turn_started`に`launch`を書く（下の[runtimeのplannerの経路](#runtimeのplannerの経路)）
- **inbox**: 今は`inbox_opened`がproviderを持つだけで、`launch`を持たない（[`up` / `down`](up-down.md)の「`dagq inbox`」）。
  ADR-t1181-1決定5は、起動の記録に選んだ場所と要求したmodelを、区間に実際のmodelとsession（thread）のidを足し、stats / kpiでproviderごとに読むと決めた（実装前）

計測の層別はtranscriptから読んだ`session_closed`の値を使う。
`launch`は起動の意図と出どころで、既定で起動したsessionの実際の値はtranscriptが持つ。

- **実際のmodelをjobの終わりのeventに写さない**: 実際の`model` / `effort`は区間の`session_closed`だけが持ち、jobの終わりのeventには写さない。
  `session_closed`はjobの終わりのeventと同じトランザクション・同じ時刻に書かれ、開始のevent（`launch`）ともjobとも結べる（`domain::sessions`）ので、写すと同じ値を2か所に持つだけになる。
  `stats`と`kpi`のsessionの層も`session_closed`を読む
- **Codexのjobの実際のmodelとthread**: Codexはsessionのidをrunを始める前に受け取らず（threadを自分で名付ける）、transcriptも無いので、Codexのjobは開始のeventの`session_id`をnullにし、終わりのeventにthreadのid・rolloutから読んだ実際のmodel・読めなかった理由を写す（`domain::headless_job::JobSession::record`）。
  区間の`session_closed`も同じ値を持つ（[Agent provider lifecycle](../provider-lifecycle.md#claude-sessionの区間)）。
  上の「写さない」はClaudeのjobの話で、Codexのjobは終わりのeventしか値の出どころが無いので写す。
  `stats`のjobの`by_model`は、開始の`session_id`の`session_closed`が無ければ終わりのeventの`model`を読む
- **`headless_jobs.provider`**（`migrations/0055_headless_job_provider.sql`）: supervisorが起動するheadlessのjobのプロセスの行に、行き先で実際に起動したprovider（launchの`provider`。切り替えた後ならその先。`JobSubject::provider`）を書く（古い行の読み方は`JobSubject::provider`のdoc comment、[Headless job processes](headless-job-processes.md)）。

## runtimeのplannerの経路<a id="runtimeのplannerの経路"></a>

runtimeのplannerは経路（対話か非対話。[ADR-t1394-2](../../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)の決定1）の選択なしに非対話だけで動く（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定3。起動とsessionは[非対話のruntimeのplanner](headless-worker.md#非対話のruntimeのplanner)）。

- 旧keyの`route`は`[roles.runtime_planner]`でだけ、値に関わらず受け付けて無視する（`domain::actor_model::ROUTE_KEY`）。
  書いてある`dagq.toml`もそのまま読める（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)決定2の切り替えの欄と同じ扱い）。
  supervisorはplannerを開くときにkeyがあれば、無視することを`warn`で1回だけlogに書く。
  他の役割の表に書けばエラーにする。
- `route`だけを書いた表は起動に何も渡さない（`RoleModels::launch`は表の無い役割と同じに扱い、既定の起動のまま）。
- 開くruntimeのplannerの`planners.route`には常に`headless`を書く（[`plan` / `planners`](plan-planners.md#runtimeのplannerの経路)）。
  `planners.route`は、過去の対話のplannerの行を`interactive`のまま読むために残る（`stats` / `kpi`の経路ごとの集計）。
- `dagq doctor`の`roles.runtime_planner`は`route: "headless"`だけを示す（`application::health::roles`）。

非対話のplannerの区間はturnから開いて閉じ、`route: headless`を持ち、`stats` / `kpi`で経路ごとに読む（ADR-t1394-2決定4。[`plan` / `planners`](plan-planners.md#runtimeのplannerの経路)、[provider-lifecycle](../provider-lifecycle.md#claude-sessionの区間)）。
wrapperはplannerのturnの`turn_started`に起動の`model`と`effort`を`launch`として書き、区間はそれを持つ。
