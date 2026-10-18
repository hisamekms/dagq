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
  - design-supervisor-lifecycle-waiting
---

# `supervise`

> supervisorはcmuxを呼ばない（[ADR-t1433-1](../../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)）。
> supervisorはlaunchdで常駐し、in-cmux modeでは新しく起動しない（[ADR-t1433-4](../../adr/2026-10-03-t1433-4-supervisor-resides-without-cmux.md)）。
> 登録済みのin-cmuxのsupervisorは、そのworkspaceに居たまま引き継ぎと`down`だけを受ける。

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

- ユースケースの本体はapplication層の`src/application/supervise/`で、submoduleの分担は`mod.rs`のmodule docが持つ（[ADR-0013](../../adr/0013-layered-architecture-and-type-function-style.md)の方針1と8）。
- `supervise(&Ports, &LoopSettings)`が起動とループを行い、外のものには`Ports`のport越しにだけ触れ、pathと子プロセスの環境は`Layout`として値で受ける。
- `runtime::supervise`はadapterを組み立てて呼ぶ入口だけ。
- promptと依頼文は`src/application/prompt/`、askの登録は`application::ask`（通知はせずinboxのwatchが知らせる）。
- watcherの居ないinboxへの知らせは`inbox_nudge.rs`（[inboxへの知らせ](ask-push.md#supervisorによるinboxへの知らせ)）。
- 計画管理・観測と分析・host運用の状態は`contexts.rs`（登録は`handoff.rs`）にあり、そのpassだけが変える。
- 実行と着地の状態は`stages.rs`に工程ごとにあり、工程のsubmoduleとループが変える（欄の所有するcontextは`Supervisor`のdoc comment、[Architecture](../architecture.md)のC3）。

## 不変条件

- queue全体の排他は無く、同じqueueに複数のsupervisorが居てよい。
  1つのrunを進めるのはleaseを持つ1つのsupervisorの1つのslotだけである（[ADR-0054](../../adr/0054-run-lease-ownership-parallel-supervisors-and-recover.md)）。
- runの工程の移りは`stages.rs`の移す操作だけを通り、移るたびに記録する（[計測](../measurement.md#区間とタグ)）。
- slotの集まりは`SlotTable`だけが変える。
- 走っているrunは優先度の変更でも止めない。

## 起動とheartbeat

- 起動の順（登録の後に変更の印を記録することを含む）は`supervise`のdoc commentが持つ。
- 引き継ぎのexecで起動したときは新しく登録せず、同じtokenとPIDで登録を取り戻し、slotを組み立て直す（[Handoff](handoff.md#handoff)、`handoff.rs`のmodule doc）。
- heartbeatは`Heartbeat`と`HeartbeatPolicy`が持ち、登録と全leaseを1つの書き込みで更新する。
  busyで書けないときに止めずに書き直す範囲と、leaseがstaleになる前に自分から退く理由は`Heartbeat`と`HeartbeatPolicy`のdoc commentにある。
- heartbeatが止まったsupervisorはleaseと登録を残して終わり、stale になった後に他のsupervisorが引き継ぐ（`Heartbeat::check`）。
- slotの上限の値と出どころは`SlotLimits::resolve`が決め、登録と同じ書き込みで残す（[Run environment](run-environment.md)の`[supervisor]`）。

## passの始め

- 各passの検査と設定の読み直しの順と理由は`Supervisor::drive`のコメントが持ち、設定の変更は再起動なしに効く。
- sccacheの監視の入口は`application::supervise::sccache`、serverをsandboxの外で持つ分担は[Run environment](run-environment.md#sccacheのserver)。
- 待ちの終わったrunは新しい仕事より先に（drain中も）slotへ戻す（`return_waiting_runs`、[人の答えを待つrun](waiting.md)）。
- workerのsessionはclaimでも再開でも非対話で、supervisorはworkerの画面を読まず打ち込まない（[非対話のworker](headless-worker.md)）。

## adopt

- 死んだsupervisorのrunを引き継ぐかは`adopt.rs`の`adoptable`と`resume_adoptable`が決め、引き継ぎは`adopt_stale_runs`が行う（[ADR-0039](../../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)の決定1〜4）。
- 同じrunを2つのsupervisorが取ろうとしても`adopt_run`が1つだけを通す。
- 自分がslotで進めているrunは、leaseが他のtokenに移っていても引き継がず、slotが外れた後のpassでだけ取る（理由は`Supervisor::in_slot`のdoc comment）。
- 引き継いだrunのslotはDBとrun fileから組み立て、待ちの時間は引き継いだ時刻から数え直さない（`Supervisor::resume`、review中のrunは`adopt_review`）。
- 引き継がないrunは`recover`と下の自動のrecoverに残る。

## slotを渡す順とclaim

- `needs_session`の再開、`failed` / `interrupted`の復旧job、readyのtaskのclaimは1つの候補の列になり、taskの効く優先度の降順にslotを受ける（`domain::slot_order`のmodule doc、[ADR-t1850-1](../../adr/2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)）。
- 列を組むのは`Supervisor::fill_slots`で、種ごとの保留はその種を列から外すだけで、上位の候補のためにslotを空けて待たない。
- 効く優先度は`application::effective_priority`、基の優先度とgoal間のrankが無いことは[ADR-t1639-1](../../adr/2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)の決定2・4・8。
- 人とinboxは`set-priority`で`in_progress`のtaskの順も変えられる（[Authorization](../authorization.md)）。
- claimの規則の順は`domain::ClaimRank`（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定4）、区分の上限とagingは[claim-defer](claim-defer.md#実行区分の上限とaging)。
- claimの順は控えの記録から`candidates`・`graph`で再現できるので、順の入れ替えは記録しない。
- 候補を控える理由（衝突の多いファイル・動かせないworker・依存先を含むbuildの待ち）は[claim-defer](claim-defer.md)、claimの間隔と着地待ちが空けた軽い枠は[claim-hold](claim-hold.md)。
- claimはbase commitに着地のbranchをclaimごとに読み直すので、`integrate`で依存が解けたtaskは先行taskを含むmainから始まる。
- claimの時点の属性は`domain::measure::ClaimAttributes`、claimできる候補の標本は`domain::kpi::CandidatesSample`のdoc comment。

## provisionと監視

- provisionは`Supervisor::provision`（`session.rs`）が行い、pathを先に記録してからdirectoryとworktreeを作り、wrapperをbackgroundで起動する（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)、[非対話のworker](headless-worker.md)）。
- wrapperの環境にqueue DBのpathは無く、workerはqueue serviceのtokenでつながる（[Queue service](../queue-service.md#クライアントモード)）。
- provisioningの失敗は環境要因とみなし、以後のclaimを止め、active runをdrainしてから非0で終わる。
- 各tickの先頭で、そのrunのleaseがまだ自分のものかを確かめ、他に移っていればDBに何も書かずslotを外す（`Supervisor::disown`）。
- leaseを失うとはlease行が無いか他のtokenに移ることで、heartbeatが古いことではないので、sleepの後も誰にも引き継がれていなければそのまま続ける（[ADR-0039](../../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)の決定7）。
- sessionの見張り（wrapperの登録とheartbeat・receipt・idle marker・終了）はrunごとの`SessionWatch`、終了の依頼は`exit.rs`（[Receipt and session exit](receipt-and-session-exit.md)）。
- runごとのload averageの区間は`domain::measure::LoadWindow`（引き継いだrunの区間は`ClaimState::loads`）、hostの負荷は[hostの負荷の連続の記録](host-metrics.md)。

## 検証とreviewと着地

- sessionの終わりでrunが`validating`か`failed`のどちらになるかは`domain::run::session_end_status`（[ADR-t1594-1](../../adr/2026-10-05-t1594-1-a-receipt-left-by-a-failed-headless-turn-goes-to-validation.md)）。
- receiptの検証はrunごとのthreadで動き、ループは待たずに他のrunを見続ける（[Validation](validation.md#validation)）。
- `awaiting_integration`のrunはleaseとslotとworkerのsessionを持ったままheadlessのreviewにかかる（[Review (supervisor)](review.md#review-supervisor)、ADR-0054の決定1・6）。
- `revise`は生きているsessionへ差し戻し、それ以外の結果ではsessionが終わってからwrapperを止める（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)の決定3、[runの復旧の「後片付け」](background-recovery-job.md#後片付け)）。
- 着地は着地スロットが空くのをleaseを持ったまま待つ（[integrate](integrate.md)）。

## leaseの解放

- leaseとslotは`awaiting_integration`や`failed`になった時点でなく、runがsupervisorの手を離れて次の担い手を待つ時点で返す（ADR-0054の決定6）。
- 返す時点の一覧は`release_lease`（`src/infrastructure/runtime_store/coordination.rs`）のdoc commentが持つ。

## recoverとtriage

- 持ち主の居ないrunの自動のrecoverと復旧jobは[Triage (supervisor)](triage.md#triage-supervisor)（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定3）。
- 持ち主の居ない着地の着地スロットの解放と、worktreeに残ったプロセスの猶予は`domain::landing_release`のmodule doc。
- drainしているsupervisorも死んだ着地の解放だけは各回で行い、自分の着地を待つrunとdrainが止まらないようにする。
- 着地してよいrunが着地の列に並ぶか、もう一度reviewにかかるかは`RunHistory::landable`と`review_recovered_runs`。

## ループの終わり

- ループがいつ終わるかは`supervise`、ownerが居なくなったときの止まり方は`LoopSettings::owner`、結果の欄と登録の削除は`Supervisor::run_loop`のdoc comment。
- SIGINT/SIGTERMの1回目はclaimを止めてactive runを待ち（graceful drain）、2回目で即終了する（`install_stop_signal`）。
- 即終了したsupervisorのleaseはstaleになり、wrapperが生きているrunは次のpassで別のsupervisorが引き継ぐ（上の「adopt」）。
- 登録を消すのは自分のループの終わりと次の`up`・`down`の掃除だけで、`status` / `doctor` / `recover` / `integrate`は消さない（[`up` / `down`](up-down.md#up--down)）。

## CIの見張り（ADR-t1920-1）

[ADR-t1920-1](../../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](ci-watch.md)。
CIの見張りを持つsupervisor（`SuperviseOptions::ci_watch`）だけが、passごとに`[ci_watch]`を読み直して確かめ（[CI watch](ci-watch.md)の「いつ確かめるか」）、答えを持たないか読む手段が無い間はclaim・新しい再開・着地を保留する（同じく「読む手段が無いとき」）。
着地を始めるか・待つか・drainが人に返すかの判断は、他の保留と合わせて`domain::landing_hold::judge`が持つ。
