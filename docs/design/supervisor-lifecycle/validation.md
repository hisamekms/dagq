---
id: design-supervisor-lifecycle-validation
type: design
title: "Validation"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0040
  - adr-0029
  - adr-t963-1
  - adr-t1165-1
  - adr-t1233-2
  - adr-t2125-1
  - adr-t1925-1
  - adr-t2740-1
  - adr-t1487-1
  - design-supervisor-lifecycle-review-programs
  - adr-t2268-1
  - design-toolchain-profiles
---

# Validation

`validating`のrunのreceiptとGitの照合は、sessionと同じleaseの下でrunごとのthreadが行う。
sessionは開いたまま（[turnの後の扱い](headless-worker.md#turnの後の扱い)）でも終了済みでもよい。
受理条件と確認の順は`domain::validation::judge`、receiptの構造の約束は`Receipt::check_requiring`を入口にする。
最初に外れた項目が`last_error`になり、以降は確認しない。

宣言外のパスは`failed`でなく`needs_session`に戻す（[ADR-0029](../../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）。
差分の境界は`validation::Fact::Changes`、要求evidenceの欠けと結果の記録は`run::recorded::finish_validation`を入口にする。
evidenceの欠けも自動resumeで直させ、receiptやGitの不整合と区別する（ADR-0019決定5）。
Spikeの結果の欠けの入口は`execution_class::missing_spike_result`で、否定的な結果や未解決は欠けではなく、判定を支える中身までは検査しない（[ADR-t1487-1](../../adr/2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)決定3、[結果の約束](worker-receipt.md#spikeの結果)）。
検証の後の行き先とsessionの終了・closeの扱いは`run::history::AfterValidation`を入口にする。
成功しても着地まではtaskを`in_progress`のまま保持し、依存taskを解放しない。

containerのrunでは制御側がcloneでGitを走らせずに事実を得る（[ADR-t2179-1](../../adr/2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)決定2〜4、[Container runtime](../container-runtime.md#git)）。
事実の出どころは`application::run_git`、受け入れと拒否の境界は`application::integrate::check_receipt`を入口にする。
制御側のGitのguardが成り立たなければ、何も読まずに拒み、sessionに返さず復旧jobとtriageへ回す。
hostのworktreeの登録が壊れたときも、後のpassでは読めないためrunを失敗にして復旧へ回す。
登録が壊れていないworktreeでGitが失敗したときは検証のエラーで、次のpassがやり直す。

validatingはtaskの検証コマンドを流さず、testを含む検証は`integrate`のrebase後の1回だけにする（[ADR-0040](../../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)決定1、[検証のコマンド](integrate.md#検証のコマンド)）。
したがって検証コマンドで壊れるcommitもreviewに進み、着地の検証でsessionに戻る。
着地しないrunは下の[already_satisfiedの検証](#already_satisfiedの検証)でmainのsnapshotに検証を流す。

reviewの段の順は、[プログラムのreview](review-programs.md)、agentのreview、passの後の[着地の前のe2e](integrate.md#着地の前のe2e)である。
プログラムのreviewはtestを含まない速い形式の検査で、taskのverifyとは二重にしない。
`integrate`も検証コマンドの前に同じプログラムを着地する差分で流し直す（[reviewのプログラム](integrate.md#reviewのプログラム)）。

## already_satisfiedの検証

[ADR-t1818-1](../../adr/2026-10-05-t1818-1-worker-claims-already-satisfied-and-runtime-closes-on-a-verified-snapshot.md)の、workerが「baseがすでに受け入れ条件を満たす」と書いたrunの経路。
復旧jobの`hand_off`で戻ったcommitの無いrunも同じ経路を通る（[Triage](triage.md#verdictの適用)）。
主張はmainのsnapshotで検証し、同じheadでreviewが判定する（[Review](review.md#already_satisfiedの主張の判定)）。
着地もe2eも行わず、閉じたrunは着地とは別に集計する（[stats](commands.md)）。

snapshotの検証の入口は`application::integrate::verify_snapshot`で、着地と同じ検証の置き換えを使う（[ADR-t2740-1](../../adr/2026-10-22-t2740-1-snapshot-verification-replaces-the-coverage-gate-as-the-landing-does.md)）。
workerのworktreeを変えず、workerが書けない場所で検証し、scratchは既存のGit登録やlinkを信用せず作り直す。
containerでの検証はhostに代替せず、使えない間は受理したrunを休ませる（[fail-closedの境界](../container-runtime.md#fail-closedの境界)）。

自動で閉じる判断は`domain::satisfied_judgment::decide`を入口にする。
`high`のpassでもmainが進めば古い判定で閉じず、やり直しの上限を超えたら人に上げる。
人の答えの適用は`domain::satisfied::close_step`を入口にする。
人の答えは受け入れ条件の意味の判断なので、新しいheadで検証が通った後には聞き直さず、reviewも判定し直さない。

snapshotと結果はeventから読むので、supervisorが替わっても同じ比較になる。
再起動時のやり直しと残ったprocessの停止は`application::supervise::execution::satisfied`のmodule docを入口にする。
検証中はslotとleaseを持ち、handoffはこれを待つ。
askやcontainerの知らせで休むrunを死んだrunとして回収しない（`domain::satisfied::blocks_recovery`）。

再計画やholdが保護するtaskは、検証と判定の結果でaskを開かずtaskも閉じず、答えも適用しない。
再計画の保留とholdの解除待ちの違いは[再計画](../../plans/task-replanning.md#実装の状態)、判定のjobの終わりを待つ約束は[Review](review.md#再計画の操作が保護するrun)を参照する。

## 着地の検証

[ADR-t1925-1](../../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)に従い、着地の検証を軽くし、最終関門をmainのCIにする。
置き換えと材料の受け渡しはruntimeの汎用の仕組みで、repositoryの設定があるときだけ働く。
登録のコマンドは書き換えず着地ごとに決め直すので、設定を外せば登録のコマンドに戻る。
設定・置き換え・材料の意味は`domain::landing_verification`、設定の読み手は`infrastructure::run_env`、材料の用意は`application::integrate`を入口にする。
何を置き換え、どのtestを選ぶかはrepositoryの設定とscriptが持ち、runtimeはtestの構成や対応表の形を知らない。
失敗の分類は`domain::verify_failure`と[Toolchain profiles](../toolchain-profiles.md)を入口にする。

このrepositoryの検証はfmt・clippy・レイヤーの依存の検査・task固有の軽い検査・unit test全件と影響範囲で絞ったITを含み、行カバレッジと全部のtestはmainのCIが見る。
ITを選ぶ条件、古い表・OSの条件・helperの扱いとCIへ任せる範囲は[scripts/landing-it.sh](../../../scripts/landing-it.sh)の冒頭、値の根拠は[着地のITの絞り込みの測定](../../plans/landing-it-selection.md#決めたこと段-3b-の-adr-と-design-が使う)を入口にする。
この境界の決定は[ADR-t2892-1](../../adr/2026-10-20-t2892-1-landing-it-leaves-added-sources-and-broad-entries-to-ci.md)、[ADR-t3920-1](../../adr/2026-11-01-t3920-1-landing-it-keeps-using-an-old-table-and-widens-the-files-it-does-not-know.md)、[ADR-t3927-1](../../adr/2026-11-07-t3927-1-landing-it-widens-platform-gated-files-at-any-table-age.md)、[ADR-t3761-1](../../adr/2026-10-31-t3761-1-landing-it-selects-the-modules-a-changed-test-helper-reaches.md)にある。
[CI watch](ci-watch.md)の既に落ちているtestは着地の検証から外すが、修正task自身のtestは外さない。
一覧の渡し方は`landing_verification::KNOWN_FAILURES_ENV`を入口にする。

containerの検証では、置き換えたコマンドも登録のままのコマンドも同じcontainerで流す（[verificationの実行](../container-runtime.md#verificationの実行)）。
Linuxで流れないmacOS固有のtestはmainのCIのmacOSのjobだけに任せる（[ADR-t2182-1](../../adr/2026-10-14-t2182-1-macos-specific-tests-are-left-to-the-ci-macos-job-not-the-container-verification.md)）。
見逃した壊れはmainのCIが拾い、CIの見張りが修正taskにする。
mainの前でCIを通す仕組みと着地のrevertは無く、自動更新にもCIの確かめや全testを足さない。
全部のe2eの関門と固定バイナリを前のものに戻す手順は[Auto-update](auto-update.md)が持つ。

## runtimeが流すe2e

自動更新・install・[着地の前のe2e](integrate.md#着地の前のe2e)の関門は全部のe2eを流し、hostの道具の有無で流さないe2eを持たない（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定7、[testの制約](../../development/testing.md#e2e)）。
関門の後始末は`application::install::E2eSettings`、launchdの実行条件は`tests/e2e.rs`のmodule docを入口にする。

要否の判定と記録は`domain::validation::E2eRequirement`を入口にする（[ADR-t963-1](../../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定2・3）。
設定の読み直しと読めないときの扱いは`Supervisor::e2e_paths`、このrepositoryが指定する範囲と理由は`dagq.toml`のe2eのコメントが持つ。
workerのreceiptはe2eを裏付けず、workerは理由つきの`not_applicable`を書く。
流すのはreviewのpassの後にhostで動くruntimeの工程である（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)）。
上限内に終わり、流し直しても印で通らない失敗だけをsessionに返し、上限切れや始められないe2eは変更のせいとせずruntimeが流し直す。

commitごとの実行の要否は`domain::run_e2e::due`、resume後の扱いは`execution::resume::resume_required`を入口にする。
workerへの案内は`prompt::worker::e2e_line`のdoc commentが持つ。
手で着地させる場合にe2eを代行しない境界と確認する記録は`application::integrate::Integrator::land`のdoc commentが持つ。
