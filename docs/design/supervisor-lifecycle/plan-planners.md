---
id: design-supervisor-lifecycle-plan-planners
type: design
title: "`plan` / `planners`"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1704-1
  - adr-t1582-1
  - design-supervisor-lifecycle
  - adr-0044
  - design-persistence
  - design-supervisor-lifecycle-actor-model
  - adr-t803-1
  - adr-t1300-1
  - adr-t1394-1
  - adr-t1394-2
  - adr-t1228-1
  - adr-t1533-1
  - adr-t1404-1
  - design-supervisor-lifecycle-headless-worker
---


# `plan` / `planners`

## 目的

plannerはgoalとtaskを書いてsubmitするsessionで、runtimeだけがproposal・依頼・draft・findingごとに立てる（[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定1・6・12・13）。
人が開くplannerは廃止し、計画の入口はinboxが記録する依頼である（[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)）。

## 流れ

```text
立てる: 行 → 経路headless → ディレクトリ → wrapperをbackgroundに起動 → handleを記録
動く:   supervisorかplanner requestがturns/に依頼 → wrapperが1 turnずつ → idle marker
終わる: submit・却下・答え待ち・決めずに → 終了の依頼 → wrapperの終わり → 行を閉じてplanner_closed
```

## 責務と境界

- use caseは`src/application/planner.rs`、行の判定は`domain::PlannerSession`、記録は`planners`表（[persistence](../persistence.md)）が持つ。
- supervisorの側は`src/application/supervise/`の`plan_review.rs`（`tend_planners`）・`request_planner.rs`・`planner_turns.rs`・`headless.rs`が持つ。
- wrapperのturnは非対話のworkerと同じ`Turns`で動く（[非対話のworker](headless-worker.md#非対話のruntimeのplanner)）。

## 不変条件

- runtimeのplannerは非対話でbackgroundのwrapperだけで動き、workspaceを開かない（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定3、[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。
- どのplannerの画面も読まず、どのplannerにも打ち込まない。
- 行を閉じるのはsupervisorだけで、plannerを閉じるCLIは無い（[ADR-t1533-1](../../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)決定2）。
- 行を閉じるのと同じトランザクションで`planner_closed`を1回だけ記録する（[ADR-t1300-1](../../adr/2026-10-02-t1300-1-runtime-closes-exited-person-planners-after-a-grace.md)）。
- runtimeが立てたplannerは上限の枠を共有し、run slotを使わない（[Draft planners](draft-planners.md)）。

## 開き方とwrapper

- `dagq plan`は何も開かずに`application::planner::PLAN_REFUSED`で拒み、権限の検査は残る（ADR-t1394-1決定1・8）。
- 開くのは`launch_planner`、wrapperは`run_planner_session`、残ったmarkerと依頼の掃除は`start_wrapper`・`prepare_planner_turns`が持つ。
- model・effortは`[roles.runtime_planner]`で決め、reviseのために開くときだけ1段上げる（`open_runtime_planner`、[Actor model](actor-model.md)）。
- reviseとreopenの初期promptは`runtime_planner_prompt`、上限は[Prompt](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)が持つ。
- handleの記録を待つ最初のturnは[非対話のworker](headless-worker.md#非対話のruntimeのplanner)の「起動」、断られたwrapperは`session::wrapper_refused`が持つ。

## 状態と生存

- 状態の判定は`planner_view`と`PlannerSession::state`、前のbinaryがworkspaceを記録した行の読み方は`retired_workspace`が持つ。
- backgroundのwrapperはpidと起動時刻で生存を見て、heartbeatの遅れでは死んだとしない（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)決定2）。
- `dagq planners`は何も閉じない（`planner_views`）。

## 終わりと片付け

- **人のplannerの終わり**: 開いたままの人のplannerの行は`close_person_planners`が閉じ、workspaceは人が閉じる（ADR-t1433-2決定5）。
  その行のreviseとanswerは新しいruntimeのplannerに届く。
- **終わりの記録**: `planner_closed`の中身は`planner_closed_payload`、codeは`PlannerCloseCode`が持つ。
  wrapperを起動できずにerror付きで閉じる行は記録しない。
- **片付け**: 行はsweepの`close_abandoned_planners`、`runner`は`remove_unused_planner_runners`が片付ける。
  閉じてwrapperもagentも動いていないplannerのdirは`[retention]`の保持期間の後にsweepが消し、行は残る（[Persistence](../persistence.md#queue-location)）。

## runtimeのplannerの経路<a id="runtimeのplannerの経路"></a>

- 経路は非対話だけで、選ぶ設定は無い（[ADR-t1394-2](../../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)決定1・2、ADR-t1433-2決定3）。
  旧keyは[Actor model](actor-model.md#runtimeのplannerの経路)が持つ。
- **turnの依頼**: supervisorの依頼は全て`send_to_planner`を通り、前のidle markerを数えないよう`stamp_planner_input`が入力の印を書く。
- **状態**: idleの判定は`planner_view`と`turn::idle_after_turn`、終了の依頼の前の読み直しは`PlanningState::request_outstanding`が持つ。
  届けたanswerを読み終えるまで終了の依頼を置かない（判じ方は[Draft planners](draft-planners.md#予定-人のplannerの廃止と非対話の経路)）。
  落とし穴: 壁で失敗したturnの後に終わらせるとanswerが失われる。
- **時間切れ**: plannerの時間はturnの`[stall]`の上限だけで切られる（`tell_of_stopped_planner_turns`）。
  idleのまま仕事が終わらないものは、reviseの時間切れが見る（[Plan review](plan-review.md#reviseの配送)）。
- **人の答えだけを待つplannerの終わり**: 残るのが自分の未回答の`planner_question`（と並んで渡されたrevise）だけなら、終了の依頼を置いて枠を空け、決めずに終わった回数に数えない（[ADR-t1704-1](../../adr/2026-10-05-t1704-1-human-answer-wait-releases-runtime-planner-slots.md)、`planner_answer_wait`）。
  answerは新しいplannerの初期promptで運ばれる（`PlannerHandover`）。
- **使えないとき**: Claudeの壁で終わったplannerは終わらせずに控えを待ち、解けたら`provider retry`で流し直し、providerは切り替えない（`planner_turns.rs`、ADR-t1394-2決定5）。
  壁の後に取る依頼の順は`turn::request_to_take`が持つ。
- **区間**: turnから稼働時間とtokenを取る（`domain::sessions::RUNTIME_PLANNER`、[計測](../measurement.md)）。

### 続きの依頼と非対話のplannerのCLI<a id="続きの依頼と非対話のplannerのcli"></a>

- 開いているplannerへの続きの依頼は`dagq planner request`で、宛先はplannerのIDである（`application::planner_request`、ADR-t1533-1）。
  拒む条件と権限の検査の順はmodule doc、入力の読み方は`src/main.rs`の`request_words`が持つ。
- 開いたplannerの居ない依頼に言葉を足すときは、新しい依頼を`request add`で記録する。
- `planner screen`・`planner send`は[非対話のworker](headless-worker.md#非対話のruntimeのplanner)の「CLI」、出力の`planner log`は[同じ文書](headless-worker.md#workspaceなしのbackgroundのwrapper)が持つ。

## inboxからの計画の依頼<a id="inboxからの計画の依頼"></a>

人がinboxに頼んだ計画は依頼として記録し、supervisorが依頼ごとにruntimeのplannerを立て、plannerがsubmitか理由付きの却下まで進める（ADR-t1394-1の決定2〜7と10）。
人が要る判断は`planner_question`だけでinboxに上がる。
記録は[Persistence](../persistence.md#planning-requestsplan_requestsrs)、規則は[Domain model](../domain-model.md#planning-requests)が持つ。

1. **記録**: `request add`が人の言葉を記録し、中身は記録の後に変えない（`domain::plan_request`）。
   打てるのはinboxと`DAGQ_ROLE`の無い人だけである（`request.record`）。
   優先度はruntimeが依頼の値をgoalとtaskに付け、plannerは書き写さない（[ADR-t1975-1](../../adr/2026-10-08-t1975-1-runtime-records-origin-and-person-set-priority-and-ai-actors-cannot-change-it.md)）。
2. **立てる**: 対象は`plan_requests.rs`の`TARGETS`、順と枠は`open_request_planners`が持つ。
   同じdraftに依頼と再検討のplannerを同時に立てない（[Draft planners](draft-planners.md#再検討の時刻)）。
3. **受け渡しとprompt**: 人の言葉はファイルで渡し（`planner_handoff::hand_request_to_planner`）、初期promptは`prompt::request_planner_prompt`で、上げる基準は[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)である。
4. **結末**: submitがproposalを依頼に結び（`link_requests`）、却下の`request decline`は依頼のplanner自身だけが打てる。
   決めずに終わったplannerが上限に達した依頼は`exhausted`になる（`MAX_REQUEST_PLANNERS`）。
5. **`planner_question`**: 依頼のaskを開けるのはその依頼の閉じていないruntimeのplanner自身だけである（[Authorization](../authorization.md#対話と記録のコマンドapplication)）。
   answerの行き先は`plan_requests.rs`の`route_of`、配送は`request_planner.rs`が持つ。
6. **終わりと時間切れ**: 上の[runtimeのplannerの経路](#runtimeのplannerの経路)に従う。

### Spikeの再計画の依頼<a id="spikeの再計画の依頼"></a>

- Spikeが`completed`になると、runtimeがSpikeごとに1件の依頼を記録し、上の2〜6の経路で再計画する（[ADR-t1487-1](../../adr/2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)決定5・6・8）。
  記録と欠けの補いは`infrastructure::plan_requests`が持つ。
- 同じgoalを再計画する依頼（Spikeと再計画の操作）は、answerによる開き直しとreviseも含めて1件ずつ進める（[ADR-t1521-2](../../adr/2026-10-05-t1521-2-trusted-runtime-parks-snapshots-and-atomically-replaces-tasks.md)決定3）。
  待ち方は`REVISE_WAITS_FOR_GOAL`、待つreviseの時間切れの起点は`planner_wait_start`が持つ。
- 頼んだ人が居ないので、attentionにするのは`request_planner_exhausted`だけである（`domain::event_attention`）。
- goalのSpikeの周期の数え方と上限（`[goals] max_spike_cycles`）でのruntimeのplannerのsubmitの拒みは`domain::investigation::spike_submit`が持つ（[ADR-t2414-1](../../adr/2026-10-19-t2414-1-spike-cycles-count-each-replanning-round-even-inside-one-investigation.md)、[claimを控える](claim-hold.md#調査中の計画の上限)）。
  拒まれたplannerは人に上げ、runtimeは未解決を理由にSpikeを延長も連鎖もしない。

## 予定: inboxからの依頼と非対話のplanner<a id="予定-inboxからの依頼と非対話のplanner"></a>

- `status`のinbox向けの欄にplannerを待っている`open`の依頼を出すのは未実装で、今は`dagq requests`で読む（ADR-t1394-1のConsequences）。

## taskの再計画の依頼

人の`replan request`は再計画の操作に結んだ依頼を記録し、操作が完全なsnapshotを持つまで立てる対象から外す（`plan_requests.rs`の`TARGETS`）。
進み方と残りの予定は[taskの再計画](../../plans/task-replanning.md)が持つ。

- 約束: 操作が閉じても依頼を`open`のまま残さず、計画されない依頼のaskを人に残さない。
  開いているplannerは途中で止めず、submitか却下で決めさせる。
  入口は`replans.rs`の`move_in`と`plan_requests.rs`の`decline_after_planner`・`decline_for_operation`・`renew_declined`である。
- 依頼のplannerが立つと操作を`planning`へ進め（`replans.rs`の`planner_opened`）、依頼の結末から操作を進めるのは`settle_replan_requests`である。
