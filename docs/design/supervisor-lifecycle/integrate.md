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
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-landing-branch
  - adr-0008
  - adr-0067
  - adr-0013
  - adr-0017
  - adr-0029
  - adr-0040
  - adr-0068
  - adr-t1925-1
  - adr-t2268-1
  - design-toolchain-profiles
---

# `integrate`

## 目的

検証済みのrunを、今の`main`へrebaseし、検証のコマンドを1回流して、1つのsquash commitで着地させてpushする（[ADR-0008](../../adr/0008-merge-queue-squash-landing.md)）。
supervisorはreviewがpassしたrunと、上限の後の差し戻しを収束と判定したrunを着地させる（[Review](review.md#review-supervisor)）。
人やinboxが`dagq integrate`を打つのは、reviewが失敗したrunと、review以前のsupervisorが残した着地待ちのrunと、着地が控えたrunである。

## 流れ

```text
承認: 束縛の確認 → runの選択 → leaseと空き容量の確認 → 着地の承認の記録 → slotを取る
着地: 着地済みか → worktreeの前処理 → receiptの検査 → rebase → 番号の振り直し → pathsの検査 → 検証のコマンド
      └─ 通れば squash commit → mainを進める → 完了 → 後始末 → push → follow_upsの登録
      └─ 通らなければ needs_session / held（hostの失敗） / failed（receiptが失敗を報告）
```

## 責務と境界

- ユースケースの本体は`src/application/integrate.rs`で、module docが範囲を持つ（[ADR-0013](../../adr/0013-layered-architecture-and-type-function-style.md)の方針1と8）。
- 着地とpushは`Integrator`だけが行い、supervisorと人の`integrate`は`IntegrationRequest`を渡す（[ADR-t728-2](../../adr/2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)）。
  依頼者の検査と記録は`landing_refusal`と[Authorization](../authorization.md#着地とpushintegrator)が持つ。
- 承認の段は`Integrator::approve`（本体は`begin`）、着地の段は`Integrator::land`（本体は`land_integrating`と`land`）が入口で、どちらもportの束`Integration`だけを受ける。
- 人の`integrate`の入口は`compose::OneShot::integrate`（`src/compose/execution.rs`）で、queueとrepositoryを開き、承認と着地の間はleaseを生かす。
- supervisorの着地は`src/application/supervise/landing.rs`が同じ`Integrator::land`へ依頼する。
- validationの`check_receipt`も同じfileにあり、着地のreceiptの検査と判定を共有する（[Validation](validation.md#validation)）。
- 結果は`IntegrationOutcome`で、各outcomeの意味はそのdoc commentが持つ。

## 不変条件

- queueで同時に`integrating`のrunは1件だけで、slotを持つ間は`main`が動かない（`begin_integration`）。
- 同じrunを二重に着地させない: 前の着地が`main`を進めてから記録の前に止まったときは、そのcommitを着地として記録する（`landed_before`）。
- `main`が動く前のerrorはslotを返し、runを元のstatusに戻す。
  `main`が動いた後の後始末・push・follow_upsの失敗は着地を変えない。
- 検証のコマンドはcommitごとにここで1回だけ流れ、validatingは流さない（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定1）。
- squashして載せる差分（main..rebase後のhead）がtaskの`paths`に収まらなければ着地しない（[ADR-0029](../../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)の決定4）。

## 承認とslot

- 束縛・対象のrun・supervisorのlease・人の`integrate`の空き容量の確かめと、着地の承認の記録は`begin`が持つ（空き容量は[空き容量](disk-space.md#人が打つintegrate)）。
- supervisorがreviewかresumeで持っているrunは、人の`integrate`では承認しない。

## receiptの検査とrebase

- worktreeの前処理・receiptの照合・rebaseの順と、それぞれが`needs_session`にする理由は`land`のコメントが持つ。
- 前の着地がrebaseの後に止まって残したheadは、receiptの`commit`と違っても照合に通る（`RunHistory::landing_rewrote`）。
- 読んだreceiptは着地の成否によらず記録し、sessionが書き直したreceiptをDBが持つのはこの記録だけである。
- receiptが`failed`か`already_satisfied`ならrunを`failed`にして終える（`receipt_ends_landing`、[Validation](validation.md#already_satisfiedの検証)）。
- queueのdirectoryを移したrunのworktreeは、後始末で消せるように先に修復する（[ADR-0017](../../adr/0017-resolve-run-paths-from-the-queue-directory.md)）。

## migrationの番号の振り直し

- rebaseの後にrunが足したmigrationの番号が`main`のものと重なれば、機械で直せる1件だけを振り直し、他は`needs_session`にする（`plan_renumber`・`renumber_migration`、[ADR-0067](../../adr/0067-migrations-are-listed-by-build-and-renumbered-on-landing.md)の決定3）。
- 振り直しはqueueのrepositoryがdagqのソースのときだけ動き、他のrepositoryの`migrations/`は普通のファイルとして扱う（[ADR-t614-1](../../adr/2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)、[Source repository](source-repository.md)）。
- Gitが振り直しのcommitを拒めば、renameを戻して理由をsessionに渡す（`renumber_migration`のコメント）。

## 検証のコマンド

- 1つの試行の流れは`land`の検証のloopと`VerifyStep::run`が持ち、logは試行ごとに別の名前で前の試行を上書きしない（`integrate_verify_log`・`next_integrate_attempt`）。
- コマンドの起動・時間切れ・process groupの止め方は`infrastructure::adapters::run_shell_to_log`、環境は同じmoduleの`verification_command`のdoc commentが持つ（[Run environment](run-environment.md#run-environment)）。
- 失敗の分類と、印を当てる順とその理由は`domain::verify_failure::classify`が持つ。
  印の出どころ（runtimeの共通・宣言したtoolchainのprofile・`dagq.toml`のprojectの印）は[Toolchain profiles](../toolchain-profiles.md)にある。
- Rustのprofileの印とnextestの失敗のstatusの集合は`domain::toolchain::rust`のdoc commentが持ち、同じ集合を持つscriptとCIのjobをそろえて直す約束もそこにある。
- コードの分類の失敗は`needs_session`になり、supervisorが自動でresumeする（`verification_failed`）。
- 着地の検証を`dagq.toml`が持てば、それが名指すtaskのコマンドを置き換え、登録は変えない（`domain::landing_verification::plan`・`landing_env`、[ADR-t1925-1](../../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)、[Validation](validation.md#着地の検証)）。
  下の2つのやり直しは置き換えたコマンドにも効く。
- verificationがcontainerなら、コマンドはrebase後のcommitのcloneのcontainerで流れ、使えなければコマンドの前に止める（[Container runtime](../container-runtime.md#verificationの実行)、[fail-closedの境界](../container-runtime.md#fail-closedの境界)）。

## hostの失敗のやり直し

- hostの分類（`FailureClass::is_environmental`）の失敗は、workerをresumeせず同じ試行の中で1回だけやり直す（[ADR-t639-1](../../adr/2026-09-27-t639-1-retry-verification-failures-of-the-host-once.md)）。
- やり直しもhostの分類で落ちるか、`disk_full`でやり直す空きが無ければ、runをresumeせずにleaseの無い着地待ちへ控える（`held`）。
- 控えたrunはinboxのattentionになり、人がhostを直して`dagq integrate`を打てば新しい試行で検証からやり直す（`domain::run_attention`）。
- 分類の集合は人の判断なしに広げない（ADR-t639-1）。

## 不安定なtestの着地のやり直し

- 落ちたtestが全て道具の流し直しで通った失敗（`flaky`）は、resumeせず同じslotと同じheadで検証を最初からもう1回流す（[ADR-t768-1](../../adr/2026-09-27-t768-1-rerun-failed-tests-once-and-land-again-on-flaky-only.md)）。
- やり直しは着地の試行ごとに1回までで、宣言したprofileのやり直しのenvを渡す（[ADR-t1039-1](../../adr/2026-09-30-t1039-1-retry-flaky-verification-per-landing-and-record-passing-flakes.md)、`ToolchainProfile::flaky_retry_env`）。
  receiptの照合とrebaseをやり直さない理由は`land`の検証のloopのコメントにある。
- 流し直しと流しきりは`.config/nextest.toml`の設定（理由はそのコメント）で、CI・人の手元・workerのstressにも効く。
  落ちた時点で止まらず全部を流しきるので、本当の失敗を`flaky`と見誤らない。
- nextestでない`cargo test`の検証には効かず、`flaky`にもならない。
- やり直しはattentionにせず、inboxとobserverは記録と[`stats`](commands.md)で読む。

## 着地とpush

- squash commitを作るのは`land`の終わり、messageは`commit_message`、`main`の進め方は`Repository::advance_main`、完了の記録は`finish_integration`が持つ。
  runの元の履歴は`refs/dagq/runs/<run-id>`に残る。
- 着地先のbranchは着地の開始時に1度だけ解決し、着地したcommitが設定を書き換えても同じbranchへ進めてpushする（[Landing branch](landing-branch.md)、[ADR-t615-1](../../adr/2026-09-27-t615-1-landing-branch-and-push-remote-per-repository.md)）。
- 着地の後に、そのrunとtaskの答えの要らなくなったaskを閉じ、worktreeとbranchを消す（`close_landing_asks`・`remove_landed_worktree`）。
- pushの先・結果・記録は`decide_push`が決め、pushが失敗してもrunは着地したままで`integrate`は成功で終わる。
  失敗したpushはattentionになり、人が原因を直してpushする（手当てはpluginの`dagq-recover`の`reference/review-by-hand.md`の「A failed push」、pushの先は[Landing branch](landing-branch.md)）。
- `integrate`の間のrunの工程の記録は[計測](../measurement.md#区間とタグ)が持つ。
- containerで動くrunは制御側の着地用のworktreeで着地し、作れないかgitのhookとdriverを止められなければ控える（[Container runtime](../container-runtime.md#git)）。

## follow_upsの登録

- 着地したreceiptの`follow_ups`は`register_follow_ups`が1つのtransactionでdraft taskにし、その規則（goal・深さ・出どころ・分類・所属の提案・二重登録の防ぎ）はそのdoc commentが持つ（[ADR-t947-3](../../adr/2026-09-28-t947-3-follow-ups-carry-category-codes.md)、[follow_upsの分類コード](receipt-and-session-exit.md#follow_upsの分類コード)）。
- draftはclaimされず、同じrunのfollow_upの束ごとに立つplannerが扱う（[Draft planners (supervisor)](draft-planners.md#draft-planners-supervisor)、[所属の判断](../follow-up-membership.md)）。

## landing recheck

- `main`が最後のrecheckから動けば、supervisorの着地に限らず直接の`integrate`・handoffや再起動の間の着地・dagqを通さないpushの後も、次のpassで着地待ちのrunを確かめる（[Landing recheck](landing-recheck.md)、[ADR-0068](../../adr/0068-recheck-waiting-runs-after-each-landing.md)、[ADR-t1310-1](../../adr/2026-10-03-t1310-1-recheck-whenever-main-moves-past-the-last-recheck.md)）。
