---
id: design-supervisor-lifecycle-integrate
type: design
title: "`integrate`"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t728-2
  - adr-t639-1
  - adr-t768-1
  - adr-t3344-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-validation
  - design-supervisor-lifecycle-run-environment
  - adr-0008
  - adr-0067
  - adr-0013
  - adr-0017
  - adr-0029
  - adr-0040
  - adr-0049
  - adr-0068
  - adr-t615-1
  - adr-t1233-2
  - adr-t1310-1
  - adr-t1311-1
  - adr-t1925-1
  - adr-t2257-1
  - adr-t2268-1
  - adr-t2424-1
  - adr-t2719-1
  - design-toolchain-profiles
---

# `integrate`

## 目的

検証済みのrunを、今の`main`へrebaseし、検証のコマンドを1回流して、1つのsquash commitで着地させてpushする（[ADR-0008](../../adr/0008-merge-queue-squash-landing.md)）。
supervisorはreviewがpassしたrunと、上限の後の差し戻しを収束と判定したrunを着地させる（[Review](review.md#review-supervisor)）。
人やinboxが`dagq integrate`を打つのは、reviewが失敗したrunと、review以前のsupervisorが残した着地待ちのrunと、着地が控えたrunである。

## 流れ

```text
reviewのpass → 着地の前のe2e（要るrunだけ） → 着地slot
承認: 束縛の確認 → runの選択 → leaseと空き容量の確認 → 着地の承認の記録 → slotを取る
着地: 着地済みか → worktreeの前処理 → receiptの検査 → rebase → 番号の振り直し → pathsの検査 → 検証のコマンド
      └─ 通れば squash commit → mainを進める → 完了 → 後始末 → push → follow_upsの登録
      └─ 通らなければ needs_session / held（hostの失敗） / failed（receiptが失敗を報告）
mainが動く → landing recheck → 着地しなくなった待ちのrunをresumeへ
```

## 責務と境界

- ユースケースの本体は`src/application/integrate.rs`で、module docが範囲を持つ（ADR-0013の方針1と8）。
- 着地とpushは`Integrator`だけが行い、依頼者は`IntegrationRequest`を渡す（[ADR-t728-2](../../adr/2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)、[Authorization](../authorization.md#着地とpushintegrator)）。
- 承認の段は`Integrator::approve`（本体は`begin`）、着地の段は`Integrator::land`（本体は`land_integrating`と`land`）で、どちらもportの束`Integration`だけを受ける。
- 人とinboxの`integrate`の入口は`compose::ControlQueues::integrate`（DBの経路は`compose::OneShot::integrate`）、supervisorの着地は`src/application/supervise/execution/landing.rs`である。
- supervisorの着地の記録は、supervisorのqueue（`SupervisorQueue`）からqueue serviceのユースケースになる（[`supervise`](supervise.md#queue-serviceの経路)）。
  slotを取るユースケースがそのrunとslotのleaseを対象にするintegratorのprincipal（`Principal::integrator`）のtokenを発行し、`Integrator::land`の間の記録はそのtokenで通り、着地が終わるとsupervisorが失効させる（ADR-t3102-1決定2）。
  着地の終わり（`finish_integration`）はそのtokenでだけ受け、supervisorのtokenとleaseでは拒む（[Security](../security.md#reviewのpassとintegrator)）。
  約束: slotはtokenを発行した同じtransactionでだけcommitし、発行できなければ取られない（`begin_landing`、ADR-t3102-3決定3）。
  tokenはその着地の`integration_started`を名指し、serviceはqueueがそのeventを持つときだけintegratorの書き込みを受けるので、commitに至らずに残ったtokenは何も書けず、片付けで失効する（着地が`integrating`を出た後の記録は受ける）。
  mainを進めることとpushは、記録のユースケースの間でIntegratorが行う。
  その後のintegratorの記録はserviceに届くまで着地のthreadで送り直し、queueは同じ記録を1回だけ取る（ADR-t3102-3決定5、[`supervise`](supervise.md#queue-serviceの経路)）。
  人とinboxの`integrate`も同じ形で、slotは自分で作ったleaseで取り、そのintegratorは`integrator:<pid>`（コマンドのプロセス）で、依頼者が失効させる（[hostのCLI](../queue-service-host-cli.md#制御側のコマンド)）。
- validationの`check_receipt`も同じfileにあり、着地のreceiptの検査と判定を共有する（[Validation](validation.md#validation)）。
- 結果の意味は`IntegrationOutcome`のdoc commentが持つ。
- e2eは`application::supervise::execution::e2e`、recheckは`application::supervise::execution::recheck`と`domain::recheck`、landing branchは`domain::landing_branch`が持つ。

## 不変条件

- queueで同時に`integrating`のrunは1件だけで、slotを持つ間は`main`が動かない（`begin_integration`）。
- 同じrunを二重に着地させない: 前の着地が`main`を進めてから記録の前に止まったときは、そのcommitを着地として記録する（`landed_before`）。
- 検証のコマンドはcommitごとにここで1回だけ流れ、validatingは流さない（ADR-0049の決定1）。
- squashして載せる差分（main..rebase後のhead）がtaskの`paths`に収まらなければ着地しない（ADR-0029の決定4）。

## 承認とslot

- 束縛・対象・lease・空き容量（[空き容量](claim-hold.md#人が打つintegrate)）の確かめと承認の記録は`begin`が持ち、supervisorがreviewかresumeで持つrunは人の`integrate`では承認しない。
- 検証のコマンドは自動更新のjobの関門（buildとe2e）と同時に流れず、関門が走るか番を待つ間、supervisorは着地を次のpassに回し、`land_integrating`は番を取れるまで待つ（ADR-t2719-1、[Auto-update](auto-update.md)の「着地の検証との順番」）。

## receiptの検査とrebase

- worktreeの前処理・receiptの照合・rebaseの順と、それぞれが`needs_session`にする理由は`land`のコメントが持つ。
- 前の着地がrebaseの後に止まって残したheadは、receiptの`commit`と違っても照合に通る（`RunHistory::landing_rewrote`）。
- 読んだreceiptは着地の成否によらず記録し、sessionが書き直したreceiptをDBが持つのはこの記録だけである。
- receiptが`failed`か`already_satisfied`ならrunを`failed`にして終える（`receipt_ends_landing`、[Validation](validation.md#already_satisfiedの検証)）。

## migrationの番号の振り直し

- rebaseの後にrunが足したmigrationの番号が`main`のものと重なれば、機械で直せる1件だけを振り直し、他は`needs_session`にする（`plan_renumber`・`renumber_migration`、[ADR-0067](../../adr/0067-migrations-are-listed-by-build-and-renumbered-on-landing.md)の決定3）。
- 振り直しはqueueのrepositoryがdagqのソースのときだけ動く（[Source repository](source-repository.md)）。

## 検証のコマンド

- 1つの試行の流れは`land`の検証のloopと`VerifyStep::run`が持つ。
- コマンドの起動と止め方は`infrastructure::adapters::run_shell_to_log`、環境は`verification_command`が持つ（[Run environment](run-environment.md#run-environment)）。
- 失敗の分類は`domain::verify_failure::classify`、印の出どころは[Toolchain profiles](../toolchain-profiles.md)、Rustの印とnextestのstatusの集合は`domain::toolchain::rust`が持つ。
- `dagq.toml`の着地の検証は、名指すtaskのコマンドを置き換える（ADR-t1925-1、[Validation](validation.md#着地の検証)）。
  下の2つのやり直しは置き換えたコマンドにも効く。
- verificationがcontainerなら、コマンドはrebase後のcommitのcloneのcontainerで流れ、使えなければコマンドの前に止める（[Container runtime](../container-runtime.md#verificationの実行)）。

## hostの失敗のやり直し

- hostの分類（`FailureClass::is_environmental`）の失敗は、workerをresumeせず同じ試行の中で1回だけやり直す（ADR-t639-1）。
- なおhostの分類で落ちるか、やり直す空きが無ければ、leaseの無い着地待ちへ控え（`held`）、inboxのattentionになる。
  人がhostを直して`dagq integrate`を打てば、新しい試行で検証からやり直す（`domain::run_attention`）。
- 分類の集合は人の判断なしに広げない（ADR-t639-1）。
- 例外: handoffのdrainのあいだはやり直さず、runをleaseを持ったまま着地slotの待ちとして次のプロセスに引き継ぎ、次のプロセスが新しい試行で検証する（ADR-t3344-1、[Handoff](handoff.md)の「区切り」）。
  止めるdrainは同じ試行でやり直す。

## 不安定なtestの着地のやり直し

- 落ちたtestが全て道具の流し直しで通った失敗（`flaky`）は、resumeせず同じslotと同じheadで検証を最初からもう1回流す（ADR-t768-1）。
- やり直しは着地の試行ごとに1回までで、宣言したprofileのやり直しのenvを渡す（ADR-t1039-1）。
- 例外: handoffのdrainのあいだはやり直さず、runをleaseを持ったまま着地slotの待ちとして引き継ぎ、次のプロセスの新しい着地がrebase後のheadで検証し、1回をそこで数え直す（ADR-t3344-1）。
- 落とし穴: nextestでない`cargo test`の検証は`flaky`にならない。
- やり直しはattentionにせず、記録と[`stats`](commands.md)で読む。

## 着地とpush

- squash commitは`land`の終わりが作って`Repository::advance_main`が`main`を進め、runの元の履歴は`refs/dagq/runs/<run-id>`に残る。
- pushの先・結果・記録は`decide_push`が決め、pushが失敗してもrunは着地したままで`integrate`は成功で終わる。
  失敗したpushはattentionになり、人が原因を直してpushする（pluginの`dagq-recover`の`reference/review-by-hand.md`の「A failed push」）。
- containerで動くrunの着地用のworktreeは[Container runtime](../container-runtime.md#git)が持つ。
- 着地の後始末は、着地したrunに紐づく閉じていないaskをkindを問わず閉じる（`close_landing_asks`）。
  着地したrunの問いはもう意味を持たないので、`worker_question`の未回答のものも回答済みで配送されていないものも`dagq asks`とinboxに残さない。
  未回答のものはruntimeが着地で閉じた旨を答え（`runtime_closed: true`の`ask_answered`）、回答済みのものは答えを適用せず`ask_closed`で閉じ、`stalled`のaskは終わっていない検出の終わり（`stall_resolved`、未回答なら`run_ended`）も記録する。
  runを名指さないaskは、そのtaskの`blocked`だけを閉じ、queueのaskは閉じない。

## landing branch

- 着地先のbranch・pushのremote・pushするかは`dagq.toml`の`[repository]`が持つ（ADR-t615-1）。
- 解決は保存せず使うたびに行い、1回の着地は開始時に1度だけ解決する（`LandingBranch`）。
- sessionやaskの文面は「main」の代わりに解決したbranchの名前を書く。
- `up`のpreflight・`supervise`の起動・`integrate`は解決できなければ止まり、`doctor`は同じerrorを報告し、既定のbranchを仮定しない。
  走っているsupervisorは解決できない間、claimと着地を始めずに控え、その変わり目だけを記録する（`check_landing_branch`）。
- supervisorが解決し直す時は`check_landing_branch`のdoc commentが持つ。

## 着地の前のe2e

- e2eはworkerが流さず、reviewがpassしたrunのうち要るもの（[Validation](validation.md#runtimeが流すe2e)）に、着地slotの前にsupervisorがhostで流す（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)）。
  着地のrebaseの後と人の`integrate`では流さない。
- 自動更新の関門（[Auto-update](auto-update.md)）と同じ`e2e_gate`で、hostで1本ずつ流す。
  隔離の印はlanding branchのcommitから読むので、workerが足した印は効かない。
- 印で通らない失敗は`needs_session`のresumeになり、流せなかったときは変更のせいとせず、slotとleaseを持ったまま待って流し直す（[`needs_session`](needs-session.md#needs_session)）。
- dagqのsourceでなければ流さずに着地するのは今の実装の制約で、決まった形は`dagq.toml`が宣言したコマンドを流す（ADR-t2257-1決定4）。
- 落とし穴: e2eに渡すenvは許す名前に絞るので、足りない版が固定バイナリになると全てのrunのe2eが始まらないか落ち続ける（見分け方と戻し方は`infrastructure::e2e_gate::PASSED_ENV`）。

## landing recheck

- `main`が最後のrecheckから動けば（dagqの外のpushも）、次のpassで着地待ちのrunを先回りして確かめる（[ADR-0068](../../adr/0068-recheck-waiting-runs-after-each-landing.md)、ADR-t1310-1）。
- 確かめ方、着地しなくなったrunのresumeとpark、結果の記録（ADR-t1311-1）、hostの分類の失敗とcontainerが無いときに判定しないこと（ADR-t639-1、ADR-t2424-1）、build outputの書き込みの確認（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定9）は、`application::supervise::execution::recheck`のmodule docと`domain::recheck::TargetCheck`が持つ。
- コードの分類で落ちたrunは、mainの木だけでも落ちればresumeせず着地を待ち続け、inboxに1回だけ知らせる（[ADR-t2141-1](../../adr/2026-10-20-t2141-1-recheck-checks-main-alone-before-resuming-runs-whose-command-failed.md)、`domain::recheck::MainCheck`・`kept_by_main`）。
- recheckのerrorはwarnだけで着地を止めず（`recheck_pass`）、mainの動きを見るsupervisorは選ばず、drainやhandoffの間は生きているほかのsupervisorが確かめる。
- recheckも同じ番を取り、関門の間は始めずに次のpassで見直す。
- 着地待ちのrunが無ければ番もrecheckのlockも取らず、そのlockの下でだけbuild outputを消す空き容量の掃除（[queueのdirのビルドのキャッシュ](claim-hold.md#queueのdirのビルドのキャッシュ)）を妨げない（`look_before_lock`）。

## errorと復旧

- `main`が動く前のerrorはslotを返してrunを戻し`integrate`は非0で終わり（`abort_integration`）、動いた後の後始末・push・follow_upsの失敗は着地を変えず、完了の記録が失敗したときはerrorが着地のcommitを名指す。
- `integrate`のprocessが途中で死んだrunは`integrating`のまま残り、`doctor`が報告し、`recover`が着地待ちに戻す。
  `integrating`のrunが残る間は、着地もlanding recheckも始まらない。

## follow_upsの登録

- 着地したreceiptの`follow_ups`は`register_follow_ups`が1つのtransactionでdraft taskにし、規則はdoc commentが持つ（ADR-t947-3、[follow_upsの分類コード](headless-worker.md#follow_upsの分類コード)）。
- draftはclaimされず、runのfollow_upの束ごとに立つplannerが扱う（[Draft planners](draft-planners.md#draft-planners-supervisor)、[所属の判断](../follow-up-membership.md)）。
