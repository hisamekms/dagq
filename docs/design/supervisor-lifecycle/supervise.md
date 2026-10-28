---
id: design-supervisor-lifecycle-supervise
type: design
title: "`supervise`"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-t1639-1
  - adr-t1591-1
  - adr-t774-1
  - adr-0013
  - adr-0054
  - adr-0053
  - adr-0039
  - adr-0049
  - adr-0047
  - design-persistence
  - adr-0071
  - adr-t3330-1
  - design-supervisor-lifecycle-waiting
  - design-queue-service
  - adr-t3102-1
  - adr-t3102-3
---

# `supervise`

> supervisorはlaunchdで常駐し、in-cmux modeでは新しく起動しない（[ADR-t1433-4](../../adr/2026-10-03-t1433-4-supervisor-resides-without-cmux.md)）。
> 前のbinaryが登録したin-cmuxのsupervisorは、引き継ぎと`down`だけを受ける。

## 目的

`dagq supervise`は常駐ループ（通常は`up`がlaunchdで起動する）で、依存が解けたtaskを上限まで同時に実行し、検証とreviewの後に1つずつ着地させ、失敗したrunを復旧jobに、止まったrunを再開に回す。
人の答えだけを待つrunはslotから外れ、slotの外で待つ（[人の答えを待つrun](waiting.md)、[ADR-0071](../../adr/0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)）。

## 流れ

```text
起動 ── 登録・heartbeat ──► pass: 読み直し → adopt → slotを渡す順（再開・復旧job・claim）
claim ──► provision ──► 監視（tick）──► sessionの終わり ──► 検証 ──► review ──► 着地
                                                     └──► needs_session / failed ──► 再開 / 復旧job
```

## 責務と境界

- ユースケースの本体は`src/application/supervise/`で、ループとsubmoduleの分担は`execution.rs`のmodule docが持つ（[ADR-0013](../../adr/0013-layered-architecture-and-type-function-style.md)の方針1と8）。
  `runtime::supervise`はadapterを組み立てて呼ぶ入口だけで、本体は外のものに`Ports`のport越しにだけ触れる。
- promptと依頼文は`src/application/prompt/`、askの登録は`application::ask`（通知はせずinboxのwatchが知らせる、[inboxへの知らせ](ask-push.md#supervisorによるinboxへの知らせ)）。
- 計画管理・観測と分析・host運用の状態は各contextのmoduleにあり、そのpassだけが変える。
  実行と着地の状態は`execution/stages.rs`に工程ごとにある（欄の所有は`Supervisor`のdoc comment、[Architecture](../architecture.md)のC3）。

## 不変条件

- queue全体の排他は無く、同じqueueに複数のsupervisorが居てよい。
  1つのrunを進めるのはleaseを持つ1つのsupervisorの1つのslotだけである（[ADR-0054](../../adr/0054-run-lease-ownership-parallel-supervisors-and-recover.md)）。
- runの工程の移りは`stages.rs`の移す操作だけを通り、移るたびに記録する（[計測](../measurement.md#区間とタグ)）。
- slotの集まりは`SlotTable`だけが変える。
- 走っているrunは優先度の変更でも止めない。

## 起動とheartbeat

- 起動の順は`supervise`のdoc comment、引き継ぎのexecで登録を取り戻してslotを組み立て直すことは[Handoff](handoff.md#handoff)と`handoff.rs`のmodule docが持つ。
- heartbeatは`Heartbeat`と`HeartbeatPolicy`で、登録と全leaseを1つの書き込みで更新し、leaseがstaleになる前に自分から退く理由はそのdoc commentにある。
- heartbeatが止まったsupervisorはleaseと登録を残して終わり、staleになった後に他のsupervisorが引き継ぐ。
- slotの上限は`SlotLimits::resolve`。

## passの始め

- 各passの検査と設定の読み直しの順は`Supervisor::drive`のコメントで、設定の変更は再起動なしに効く。
- 待ちの終わったrunは新しい仕事より先に（drain中も）slotへ戻す（`return_waiting_runs`、[人の答えを待つrun](waiting.md)）。

## adopt

- 引き継ぐかは`adopt.rs`の`adoptable`と`resume_adoptable`、引き継ぎは`adopt_stale_runs`で、同じrunは`adopt_run`が1つだけを通す（[ADR-0039](../../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)の決定1〜4）。
- 落とし穴: 自分がslotで進めているrunは、leaseが他に移っていても引き継がず、slotが外れた後のpassでだけ取る（`Supervisor::in_slot`のdoc comment）。

## slotを渡す順とclaim

- 再開・復旧job・claimは1つの候補の列になり、taskの効く優先度の降順にslotを受ける（`domain::slot_order`のmodule doc、[ADR-t1850-1](../../adr/2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)）。
- 列を組むのは`Supervisor::fill_slots`で、種ごとの保留はその種を列から外すだけで、上位の候補のためにslotを空けて待たない。
- 効く優先度は`application::effective_priority`、goal間のrankが無いことは[ADR-t1639-1](../../adr/2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)の決定2・4・8。
- claimの規則の順は`domain::ClaimRank`（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定4）、控えと区分の上限とagingは[claimを控える](claim-hold.md)。
- 順の手前で落ちるreadyのtaskは控えの記録を持たず、`application::claim_view::ClaimExclusions`が読み口で、claimはこれを読まない。
- claimはbase commitに着地のbranchをclaimごとに読み直すので、依存が解けたtaskは先行taskを含むmainから始まる。

## provisionと監視

- provisionは`Supervisor::provision`（`session.rs`）で、pathを先に記録してからworktreeを作り、wrapperをbackgroundで起動する（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。
- wrapperの環境にqueue DBのpathは無い（[Queue service](../queue-service.md#クライアントモード)）。
- provisioningの失敗は環境要因とみなし、以後のclaimを止め、active runをdrainしてから非0で終わる。
- 各tickの先頭でleaseが他に移っていればDBに何も書かずslotを外す（`Supervisor::disown`）。
- leaseを失うとはlease行が無いか他のtokenに移ることで、heartbeatが古いことではない（[ADR-0039](../../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)の決定7）。
- sessionの見張りは`SessionWatch`、終了の依頼は`exit.rs`（[receiptとsessionの終わり](headless-worker.md#receiptとsessionの終わり)）。

## 検証とreviewと着地

- sessionの終わりの行き先は`domain::run::session_end_status`（[ADR-t1594-1](../../adr/2026-10-05-t1594-1-a-receipt-left-by-a-failed-headless-turn-goes-to-validation.md)）、receiptの検証はrunごとのthreadで、ループは待たない（[Validation](validation.md#validation)）。
- `awaiting_integration`のrunはleaseとslotとsessionを持ったままreviewにかかる（[Review (supervisor)](review.md#review-supervisor)、ADR-0054の決定1・6）。
- `revise`は生きているsessionへ差し戻し、それ以外の結果ではsessionが終わってからwrapperを止める（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)の決定3）。
- 着地は着地スロットをleaseを持ったまま待つ（[integrate](integrate.md)）。

## leaseの解放

- leaseとslotは、runがsupervisorの手を離れて次の担い手を待つ時点で返し（ADR-0054の決定6）、その一覧は`release_lease`（`runtime_store/coordination.rs`）のdoc comment。
- leaseも閉じていないaskも無く最後のreviewが着地を許す`awaiting_integration`のrunは、fill passがreviewをやり直さずに着地の列に戻す（`take_up_stranded_landings`、判定は`domain::stranded_landing`、[ADR-t3049-1](../../adr/2026-10-21-t3049-1-supervisor-takes-up-a-passed-run-left-awaiting-integration-without-lease-or-ask.md)）。
  review以降に人の判断を待つ理由（ask・`integration_held`・runtimeのerror・holdや再計画・cancelなど）のあるrunは人に残す（`domain::stranded_landing`の`Keep`）。
  leaseを作る`lease_stranded_landing`はadoptと別の経路で、判定をその時点のeventで検査し直す。
- 例外: supervisorが始めた着地がcode `other`の`integration_error`でmainを動かす前に止まったrunは、errorが記録したbuildと違うbuildのsupervisorが1回拾い直す（`stranded_landing::build_retry`、[ADR-t3524-1](../../adr/2026-10-28-t3524-1-a-supervisor-of-another-build-takes-up-once-a-landing-an-error-stopped-before-main-moved.md)）。
  上限はbuildごとに1回で、最後のreview以降の履歴にそのbuildのerrorか拾い直しがあれば拾わない（A→B→Aも拾わない）。
  1つのerrorの拾い直しも1回で、最新のerrorの後にどのbuildかの拾い直しがあれば拾わない。
  `lease_stranded_landing`はleaseを取るsupervisorの登録のbuildで判定し直し、拾い直しの記録に元のerrorと両方のbuildを残す。
  そのrunの工程は記録するsupervisorのbuildで判定し、着地を止めたbuildでは人の答えを待つ工程になる。
- 落とし穴: 人を待たせるために新しくleaseを返す経路は、その理由をreview以降のeventかqueueの状態に残さないと拾い直される。

## recoverとtriage

- 持ち主の居ないrunの自動のrecoverと復旧jobは[Triage (supervisor)](triage.md#triage-supervisor)（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定3）、死んだ着地の解放は`domain::landing_release`のmodule doc。
- drainしているsupervisorも死んだ着地の解放は各回で行い、自分の着地を待つrunとdrainを止めない。
- 停止要求のdrainはleaseの無いrunの答えの済んだ`approve_landing`を適用し、承認された着地は自分のrunが残ってdrainが続くあいだだけ始める（`drain_landing_answers`、[ADR-t3330-1](../../adr/2026-10-25-t3330-1-stopping-drain-applies-landing-answers-and-lands-only-while-its-own-runs-keep-it-going.md)）。
- 着地の列に並ぶかもう一度reviewにかかるかは`RunHistory::landable`と`review_recovered_runs`。

## ループの終わり

- 終わり方は`supervise`・`LoopSettings::owner`・`Supervisor::run_loop`のdoc comment、signalは`install_stop_signal`（1回目でdrain、2回目で即終了）。
- 登録を消すのは自分のループの終わり、`up`・`down`の掃除、生きているsupervisorによる死んだ登録の片付けだけで、どれも同じtransactionで`supervisor_stopped`を書く。
  `status` / `doctor` / `recover` / `integrate`は消さない（[Persistence](../persistence.md#runtime-ownership)、[`up` / `down`](up-down.md#up--down)）。

## queue serviceの経路

supervisorはqueue DBを開かずに、claim・lease・runの状態とevent・登録・判断に使う読み取り・token・着地（integratorとして）とreviewとvalidationの記録・jobの起動と結果・計画と観測・askとattention・CIの見張り・引き継ぎの記録・containerの部品の記録と読み（imageのbuildと失敗・relayのattention・dagq machineの利用者のrun）・hostの負荷のsampleとrunの資源の記録と読み取り・ledgerの記帳（行の読み書きと保持、それが読むsessionのstepと資源と持ち主）をqueue serviceのユースケースとして行う（[ADR-t3102-3](../../adr/2026-10-22-t3102-3-stage-5-outage-notice-and-use-cases-for-multi-step-control-operations.md)決定3〜5、principalは[ADR-t3102-1](../../adr/2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)）。
supervisorはこれらのためにもqueue DBを開かない（`compose::supervisor`）。

- 入口は`application::supervisor_service`のmodule doc（serviceが答える操作、leaseのtokenの検査、actorの名指し、tokenの発行と失効）と`SupervisorQueue`、serviceの側は`answer`。
- supervisorが起動する`observe`と`throughput-review`は、そのsupervisorのtokenで同じprincipalとしてserviceを通り、DBを開かない（`compose::observation`の`job_queues`）。
  `stats`と`kpi`のGitの読み取りはCLIと同じくjobが行い、serviceはGitを起動しない（`infrastructure::job_reads`、[送る範囲](../queue-service-host-cli.md#送る範囲)）。
  引き継ぎのexecはtokenを失効させずに残す（`ServiceCaller::take_back`）ので、execをまたいで記録できる。
- 着地は`begin_landing`が発行したintegratorのprincipalで書き、serviceはそのrun・task・slotのleaseで着地の操作だけを答える（[integrate](integrate.md)）。
- 約束: 1つの要求は1つのqueueの操作で、serviceは同じ操作を同じtransactionで行う（[SQLite persistence](../persistence.md#runtime-ownership)）。
  transactionを開く・閉じる操作や行を読み書きする操作は無い。
- 約束: 届かない呼び出し（`ServiceUnreachable`）でDBを開いて代わりに行わない（fail closed）。
  起動のときは`reach_at_start`、passの途中は`Supervisor::run_loop`が扱い、その間は新しいclaimとjobを始めない。
- 約束: 結果の記録（`resend::RESENT_OPS`）はserviceが取るまで持って送り直し、queueは最初の送信の識別子ごとに1回だけ、記録した順に取る（`resend`のmodule docと`Resending`）。
  何かを始める前のrunのeventと履歴から見つけ直すものは持たず（`EventStore::record_runtime_event_now`）、取られなければ呼び出し元の誤りにする。
  ループの終わりまでに取られなければqueueの`service/held/`に残し、次のsupervisorが送る。
  持った記録が残る間は、起動も各pass（`--once`も）もqueueを読む前にそれを送り、queueが混み続ける間は待つ（`Supervisor::drive`。止まるpassは待たず、書くものをその後ろに持つ）。
  planとgoalのreviewの終わり（verdictと失敗）は`service/verdicts/`に残して同じく1回だけ記録し、reviewはやり直さない（`HeldPlanReview`・`HeldGoalReview`）。
  去ったsupervisorが残したものは、走っているsupervisorも新しいreviewを始める前に引き取る（`take_kept_verdicts`）。
- 落とし穴: 何かを始める前の記録（`revise_requested`など）と、passがqueueの履歴と比べて見つけるevent（`resend::OWN_RETRY_KINDS`、runの`provider_waiting`など）は持たない。
  前者は取られてから先へ進み、後者は持てば次の読みに見えずに二重になるので、次のpassが見つけ直す。
  例外はplannerのturnの`provider_waiting`で、結果として持つ: 見つけ直せば、間に人が答えたhold askを開き直すので（持った記録は各passが読む前に送る）。
- 落とし穴: 書いた依頼の答えが届かなければserviceが行ったかどうか分からないので、上の結果の記録のほかは送り直さず、serviceが答えた後のpassで自分のleaseのrunをslotと照合する（`execution::reconcile`のmodule doc）。
- serviceの不在の記録と知らせは[Queue service](../queue-service.md)の「落ちたときの知らせ」と`host_ops::queue_service`のmodule doc。
- testのsupervisorは`ServiceRoute::InProcess`で、CLIの`supervise`は常にsocketを通る。

## CIの見張り（ADR-t1920-1）

[ADR-t1920-1](../../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](ci-watch.md)。
CIの見張りを持つsupervisor（`SuperviseOptions::ci_watch`）だけが、答えを持たないか読む手段が無い間はclaim・新しい再開・着地を保留する（[CI watch](ci-watch.md)の「読む手段が無いとき」）。
着地を始めるか・待つか・drainが人に返すかの判断は、他の保留と合わせて`domain::landing_hold::judge`が持つ。
