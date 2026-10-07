---
id: adr-t2034-1
type: adr
title: docsだけの差分ではCIのRustのjobをjobのifで飛ばしてdocの検査は常に流し、名指したjobが飛んだsuccessの実行はmainのCIの見張りとci-failure.ymlが緑と読まない（ADR-t1920-1決定2をamends）
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-t1920-1 decision 2
owners:
  - hisamekms
tags:
  - ci
  - supervisor
  - operations
related:
  - adr-t1920-1
  - adr-t1925-1
  - adr-t2032-1
  - design-supervisor-lifecycle-ci-watch
  - design-ci-failure-issues
  - design-linux-ci
---

# ADR-t2034-1: docsだけの差分ではCIのRustのjobを飛ばし、飛ばした実行を緑と読まない（ADR-t1920-1決定2をamends）

## Context

`.github/workflows/ci.yml`はどの変更でもmacOSのjob（fmt・docの検査script・clippy・`cargo llvm-cov nextest`・Slow tests・IT test time gate）とLinuxのbuildとtestを流す。docsだけの変更でもRustの段の待ちが付き、flakyなtestで赤になればci-failureのissueと見張りのfindingのノイズになる。

ただ飛ばすだけでは2つが壊れる。(1) workflowの`on.paths-ignore`は実行そのものを起こさないので、必須のstatus checkを付けるとdocsだけのPRがpendingのまま残り、doc の検査scriptも流れなくなる。(2) [ADR-t1920-1](2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)の見張りとci-failure.ymlはworkflow全体の`conclusion`だけで緑と読む。Rustを飛ばした実行は`success`なので、赤の後のdocsだけのpushを「緑に戻った」と読み、既に落ちているtestの一覧を全部外し、issueを閉じる。次のRustを流す実行でまた赤になり、赤と緑が揺れる。

ADR-t1920-1は番号付きの決定を8つ持ち、変えるのは決定2（成否の決まらない実行を飛ばす）だけなので、[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)に従ってamendsにする。

## Decision

1. **docsだけの判定**: 比べるbaseから実行のcommitまでの差分（`git diff --name-only`）の全てのpathが`docs/**`かrootの`*.md`なら、docsだけとする。`plugins/**`（`src`が`include_str!`し`tests/plugin.rs`が読む）・`migrations/`・`.config/`・`scripts/`・`Cargo.*`・`rust-toolchain.toml`・`.github/`は含めない。baseはpull requestのbaseかpushの`before`で、無い・0だけ・履歴に無い（新しいbranchの初回・force push）とき、差分が空のとき、判定のjobが落ちたときは、docsだけとせず全部流す。判定はworkflowの中の小さなjobがoutputにし、外部のactionを足さない。baseが履歴にあるよう、判定のjobは履歴を全部取る（浅いcheckoutではbaseが無く、毎回全部流す側に落ちて黙って飛ばさなくなる）。
2. **飛ばし方はjobの`if`**: Rustのjobは判定のjobを`needs`に持ち、docsだけのときjobの`if`で飛ぶ。`needs`のjobは既定で前のjobが落ちると飛ぶので、`if`は判定のjobが落ちても流れる形に書く（判定が「docsだけ」と答えたときだけ飛ぶ）。`if`で飛んだjobはstatus checkではsuccessとして報告され（jobのAPIの`conclusion`は`skipped`で、4と6はこれを読む）、workflowの実行は起きるので、将来必須のstatus checkを付けてもdocsだけのPRはpendingで止まらず、docの検査も流れる。`on.paths-ignore`は(1)の理由で採らない。
3. **jobの分け方**: Rustの要らない検査（ADR numbers・ADR index・Frontmatter date lines・Docs frontmatter・Docs relative links・AGENTS.md size・Design docs budgets・Check scripts' root、それに安いscriptのMigration numbers・Plugin version・Test file lines・E2E quarantine marks・Layer dependencies）は常に流すjobに移す。このjobは履歴とtagを全部取る（Migration numbersは最新のreleaseのtagと、Design docs budgetsはbaseと比べる）。runnerは今と同じmacOSにし、scriptが今確かめられている環境を変えない。`cargo fmt`はtoolchainを入れる手間が要り、docsだけの差分では結果が変わらないのでRustのjobに残す。2つのRustのjobは今の`name:`を保つ。clippy・`cargo llvm-cov nextest`・JUnitの成果物・Slow tests・IT test time gateを持つmacOSのjobと`linux`のjobが同じ`if`で飛ぶ。
4. **見張りはsuccessの実行のjobを読む（ADR-t1920-1決定2に足す）**: `[ci_watch]`の`required_jobs`（jobの名前の配列）で名指したjobのどれかが`success`で終わっていない`success`の実行は、成否の決まらない実行に数える。緑に戻ったことも一覧からの外しも記録せず、次に成否の決まった実行の`skipped_runs`に数えて範囲を引き受けさせる。`required_jobs`が無ければjobを読まず今までどおり。docsだけのpushでdocの検査が落ちた赤の実行の項目（jobとstep）は、docsだけの修正の実行では外れず、Rustのjobも流して通る実行で外れる（6のissueと同じ割り切り）。runtimeは汎用なので、他のrepositoryで普段から飛ぶjob（tagだけで流すdeployなど）を巻き込まないよう、「どれかが飛んだ」ではなく名指しにする。
5. **名指したjobが無い・jobを読めない**: どちらも緑と読まず、既に落ちているtestの一覧を誤って消さない側に倒す。
   - 名指したjobが実行のjobsに無い（jobの`name:`を変えた・workflowを分けたなど）: 成否の決まらない実行に数え、設定とworkflowのずれを見張りのeventで知らせ、inbox宛てのattentionにする。同じずれは、間に名指したjobが揃った緑が無い限り繰り返し記録しない。ずれが続く間は見張りが緑を記録できず一覧が減らないので、人の手（`dagq.toml`か`ci.yml`を直すtask）が要る。`doctor`はCIの実行を読まないのでずれを知らず、知らせる口にしない。
   - jobを読めない（`gh`の失敗・時間切れ・形の読めない出力）: その実行を処理せず、見た記録も進めず、後の実行も処理せずに次の間隔で読み直す。ADR-t1920-1決定2の一時の失敗と同じ扱いで、claimも着地も止めない。すぐ成否の決まらない実行に数えると、実はjobが流れていた緑を落とす。ただし同じ実行で一時の失敗の上限まで続いたら（消えた実行・壊れた出力）、緑と読まずに成否の決まらない実行に数えて先へ進み、そのことを見張りのeventに残す。1つの実行で見張り全体が止まり、赤も見えなくなるのを避けるため。
6. **ci-failure.ymlは飛んだjobのあるsuccessで閉じない**: CIの実行のjobsに`skipped`のjobがあるsuccessの実行は、cancelされた実行と同じく何もしない（閉じない・コメントしない・「より新しい実行」の代わりにも選ばない）。jobの名前の一覧を持たずに「飛んだjobがある」で決めるのは、この repositoryの`ci.yml`で`if`を持つjobはRustのjobだけで、名前の一覧を`ci.yml`と2か所で合わせる手間が無いため。docの検査だけが落ちて開いたissueも、docsだけの修正では閉じず、Rustを流して通る実行まで残る。この規則は「`ci.yml`で`if`を持つjobはRustのjobだけ」に依るので、他のjobに`if`を足す変更は同じ変更でci-failure.ymlも直す。1件のissueの規則のまま、どのjobで開いたissueかを見分ける仕組みを作らないための割り切りで、このrepositoryではdocs以外の着地が頻繁なので残る時間は短い。
7. **CIを着地の最終関門にする方針（[ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)）と両立する**: docsだけの差分はRustのbuildとtestの入力を変えないので、Rustの段の結果はbaseの実行と変わらず、関門として失うものが無い。失うのはflakyなtestと環境の揺れの再試行だけで、それはcommitの関門ではない。4〜6で、飛ばした実行が赤を緑に塗り替えることもない。
8. 変えないもの: CIの`concurrency`、ci-failureのissueを1件にする規則、ADR-t1920-1決定2の残り（cancelと成否の決まらない終わり方を飛ばすこと、読む手段が無いときの止め方）と他の決定。

## Alternatives

- **`on.paths-ignore`**: 必須のstatus checkがpendingで残り、docの検査も消える。
- **外部のactionでpathを判定する**: 依存とsupply chainが増える。`git diff --name-only`で足りる。
- **見張りが「どれかのjobが飛んだsuccess」を成否の決まらない実行にする**: 他のrepositoryの振る舞いが設定なしに変わる。
- **名指したjobが無い実行を従来どおり緑と読む**: workflowの改名ひとつで、赤の一覧が黙って消える。
- **読めない実行を成否の決まらない実行に数えて先へ進む**: 一時の失敗で本当の緑を飛ばし、一覧が減るのが遅れる。

## Consequences

- この repositoryの`dagq.toml`に`required_jobs`を足すのは、固定バイナリがこのkeyを読めるようになってから（旧バイナリは未知のkeyでファイル全体を読めなくなる）。それまでに`ci.yml`がRustを飛ばすと、見張りは飛ばした実行を緑と読み、ci-failure.ymlはissueを閉じるので、`ci.yml`の変更は`required_jobs`とci-failure.ymlの変更の後（ci-failure.ymlは同じ変更でもよい）に着地させる。
- 見張りはsuccessの実行ごとに`gh run view`を1回多く呼ぶ（`required_jobs`があるときだけ）。成否の決まらない実行は記録しないので、次に成否の決まった実行が来るまで間隔ごとに読み直し、ずれが続く間は一覧の上限の件数まで毎回読む。
- Rustのjobの`name:`を変えるときは`required_jobs`も同じ変更で直す。漏れは5のeventが知らせる。
- 判定の書き方とjobの並びは[Linux CI](../design/linux-ci.md)、issueの扱いは[CI failure issues](../design/ci-failure-issues.md)、keyとeventは[CI watch](../design/supervisor-lifecycle/ci-watch.md)が持つ。
