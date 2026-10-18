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
- どのplannerの画面も読まず、どのplannerにも打ち込まず、plannerのためにcmuxを呼ばない。
- 行を閉じるのはsupervisorだけで、plannerを閉じるCLIは無い（[ADR-t1533-1](../../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)決定2）。
- 行を閉じるのと同じトランザクションで`planner_closed`を1回だけ記録する（[ADR-t1300-1](../../adr/2026-10-02-t1300-1-runtime-closes-exited-person-planners-after-a-grace.md)）。
- runtimeが立てたplannerは上限の枠を共有し、run slotを使わない（[Draft planners](draft-planners.md)）。

## 開き方とwrapper

- `dagq plan`は何も開かずに`application::planner::PLAN_REFUSED`で拒み、権限の検査は残る（ADR-t1394-1決定1・8）。
- 開くのは`launch_planner`で、handleを記録できないwrapperの停止はそのコメント、残ったmarkerと依頼の掃除は`start_wrapper`・`prepare_planner_turns`が持つ。
- model・effortは`[roles.runtime_planner]`で決め、reviseのために開くときだけ1段上げる（`open_runtime_planner`、[Actor model](actor-model.md)）。
- reviseとreopenのplannerの初期promptは`runtime_planner_prompt`、上限は[Prompt](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)、呼ぶ側は[Plan review](plan-review.md#reviseの配送)にある。
- wrapperは`run_planner_session`で、最初のturnは開いた側がhandleを記録するのを待つ。
  `submit`がproposalの持ち主をこのhandleから決めるためである。
- 登録を断られたwrapperはerrorで終わり、workspaceを閉じない（`session::wrapper_refused`）。

## 状態と生存

- 状態の判定は`planner_view`と`PlannerSession::state`が持つ。
- cmuxのworkspaceを記録した行はcmuxに問わず`closed`と読む（`retired_workspace`）。
- backgroundのwrapperはpidと起動時刻で生存を見て、heartbeatの遅れでは死んだとしない（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)決定2）。
- `dagq planners`は何も閉じない（`planner_views`）。

## 終わりと片付け

- **人のplannerの終わり**: 開いたままの人のplannerの行は、plannerのpassの初めに`close_person_planners`がcmuxを呼ばずに閉じ、workspaceは人が自分のterminalで閉じる（ADR-t1433-2決定5）。
  その行のreviseとanswerは新しいruntimeのplannerに届く。
- **plannerの終わりの記録**: `planner_closed`の中身は`planner_closed_payload`、codeは`PlannerCloseCode`が持つ。
  wrapperを起動できずにerror付きで閉じる行は記録しない。
- **行の片付け**: sweepの`close_abandoned_planners`がwrapperもsessionも居ない行を閉じ、supervisorが止まっていた間の分も拾う。
- **`runner`の片付け**: wrapperが終わりに消し、残りはsweepの`remove_unused_planner_runners`が消す。

## runtimeのplannerの経路<a id="runtimeのplannerの経路"></a>

- 経路は非対話だけで、選ぶ設定は無い（[ADR-t1394-2](../../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)決定1・2、ADR-t1433-2決定3）。
  `planners.route`は過去の対話の行を読むために残り、`[roles.runtime_planner]`の旧keyは[Actor model](actor-model.md#runtimeのplannerの経路)が持つ。
- **turnの依頼**: supervisorが置く依頼（revise・answer・`provider retry`・終了）は全て`send_to_planner`を通り、番号の順に取られる（`write_request`）。
  依頼の前に入力の印を書き、前のturnのidle markerでidleに見えないようにする（`stamp_planner_input`）。
- **状態**: idle markerが最後の入力より新しく、待つ依頼が無ければ`idle`である（`planner_view`、`turn::idle_after_turn`）。
  届けたanswerを読み終えたかはそのanswerの依頼を取ったturnで判じ、読み終えるまで終了の依頼を置かない（`headless_answer_taken`、`turn::request_read`）。
  壁で失敗したturnの後に終わらせると、answerが読まれずに失われる。
- **時間切れ**: plannerの時間はturnの`[stall]`の上限だけで切られ、止めたturnをinboxにplannerごとに1回知らせる（`tell_of_stopped_planner_turns`）。
  idleのまま仕事が終わらないものは、reviseの時間切れが見る（[Plan review](plan-review.md#reviseの配送)）。
- **人の答えだけを待つplannerの終わり**: 残るのが自分の未回答の`planner_question`（と並んで渡されたrevise）だけなら、終了の依頼を置いて枠を空け、決めずに終わった回数に数えない（[ADR-t1704-1](../../adr/2026-10-05-t1704-1-human-answer-wait-releases-runtime-planner-slots.md)、`planner_answer_wait`）。
  answerは行が閉じてから新しいplannerの初期promptで運ばれる（`PlannerHandover`）。
- **使えないとき**: 最後のturnがClaudeの壁（ログイン切れ・利用上限・起動の失敗）で終わったplannerは、終わらせずに控えを待ち、解けたら`provider retry`で流し直す（`planner_turns.rs`、ADR-t1394-2決定5）。
  wrapperは壁の後に`provider retry`より先に依頼を取らない（`turn::request_to_take`）。
  providerは切り替えない。
- **区間**: kind `runtime_planner`の区間はturnから稼働時間とtokenを取る（`domain::sessions::RUNTIME_PLANNER`、[計測](../measurement.md)）。

### 続きの依頼と非対話のplannerのCLI<a id="続きの依頼と非対話のplannerのcli"></a>

- 開いているplannerへの続きの依頼は`dagq planner request`で、宛先はplannerのIDである（`application::planner_request`、ADR-t1533-1）。
  拒む条件はmodule doc、入力の読み方は`src/main.rs`の`request_words`が持つ。
- 権限（`planner.request`）は入力を読む前に検査し、権限の無いactorは開いたstdinを待たない。
- 開いたplannerの居ない依頼に言葉を足すときは、新しい依頼を`request add`で記録する。
- `planner screen`と`planner send`は画面もキーも扱わない（`application::screen`）。
- backgroundのplannerの出力は`dagq planner log`で読む（[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)）。

## inboxからの計画の依頼<a id="inboxからの計画の依頼"></a>

人がinboxに頼んだ計画は依頼として記録し、supervisorが依頼ごとにruntimeのplannerを立て、plannerがsubmitか理由付きの却下まで進める（ADR-t1394-1の決定2〜7と10）。
人が要る判断は`planner_question`だけでinboxに上がる。
記録は[Persistence](../persistence.md#planning-requestsplan_requestsrs)、規則は[Domain model](../domain-model.md#planning-requests)が持つ。

1. **記録**: `request add`が人の言葉を記録し、中身は記録の後に変えない（`domain::plan_request`）。
   打てるのはinboxと`DAGQ_ROLE`の無い人だけである（`request.record`）。
   優先度はruntimeが依頼の値をgoalとtaskに付け、plannerは書き写さない（[ADR-t1975-1](../../adr/2026-10-08-t1975-1-runtime-records-origin-and-person-set-priority-and-ai-actors-cannot-change-it.md)）。
2. **一覧**: `dagq requests`。
3. **立てる**: 対象の条件は`plan_requests.rs`の`TARGETS`、立てる順と枠は`open_request_planners`が持つ。
   同じdraftに依頼と再検討のplannerを同時に立てない（[Draft planners](draft-planners.md#再検討の時刻)）。
   plannerのpassの中で、reviseとanswerの配送の後、draftとfindingのplannerの前に進めるのは、人が待っているためである。
4. **依頼の言葉の受け渡し**: 人の言葉はplannerのディレクトリのファイルに書き、promptは決まった文でそれを指す（`planner_handoff::hand_request_to_planner`）。
5. **初期prompt**: `prompt::request_planner_prompt`で、上げる基準は[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)である。
6. **結末**: submitがproposalを依頼に結び（`link_requests`）、却下の`request decline`は依頼のplanner自身だけが打てる。
   決めずに終わったplannerが上限に達した依頼は`exhausted`になる（`MAX_REQUEST_PLANNERS`）。
7. **`planner_question`**: 依頼のaskを開けるのはその依頼の閉じていないruntimeのplanner自身だけである（[Authorization](../authorization.md#対話と記録のコマンドapplication)）。
   answerの行き先は`plan_requests.rs`の`route_of`、配送は`request_planner.rs`が持つ。
8. **終わりと時間切れ**: draft・findingのplannerと同じく上の[runtimeのplannerの経路](#runtimeのplannerの経路)に従う。

### Spikeの再計画の依頼<a id="spikeの再計画の依頼"></a>

- Spikeが`completed`になると、runtimeがSpikeごとに1件の依頼を記録し、上の3〜8の経路で再計画する（[ADR-t1487-1](../../adr/2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)決定5・6・8）。
  記録と欠けの補いは`infrastructure::plan_requests`、promptは`request_planner_prompt`のSpikeの分岐が持つ。
- 同じgoalのSpikeの依頼は1件ずつ進め、goalを並行して再計画しない（`TARGETS`）。
- 頼んだ人が居ないので、attentionにするのは`request_planner_exhausted`だけである（`domain::event_attention`）。
- goalのSpikeの周期は再計画の回で数え、調査中の計画が開いたままでも、そのSpikeの依頼が`open`のあいだにruntimeのplannerが追加のSpikeをsubmitすると次の回に入る（[ADR-t2414-1](../../adr/2026-10-19-t2414-1-spike-cycles-count-each-replanning-round-even-inside-one-investigation.md)、`domain::investigation::spike_submit`）。
  周期の上限（`[goals] max_spike_cycles`）に達したgoalのSpikeはruntimeのplannerのsubmitが拒み、plannerは人に上げる（`infrastructure::proposals`、[claimを控える](claim-hold.md#調査中の計画の上限)）。
  runtimeは未解決を理由にSpikeを延長も連鎖もしない。

## 予定: inboxからの依頼と非対話のplanner<a id="予定-inboxからの依頼と非対話のplanner"></a>

- `status`のinbox向けの欄にplannerを待っている`open`の依頼を出すのは未実装で、今は`dagq requests`で読む（ADR-t1394-1のConsequences）。

## taskの再計画の依頼

- 人の`replan request`は再計画の操作に結んだ依頼を記録し、操作が完全なsnapshotを持つまで立てる対象から外す（`plan_requests.rs`の`TARGETS`）。
  残りの予定は[taskの再計画](../../plans/task-replanning.md)が持つ。
