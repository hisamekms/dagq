---
id: development-local-checks
type: development
title: このrepositoryの手元の検証（人とworkerが流すもの、testの範囲、stress、負荷の下で落ちるtestの再現、e2eを流さないこと、resumeでの再現、受け入れ条件の対応づけ、askにしないもの）
status: current
created: 2026-10-03
owners:
  - hisamekms
tags:
  - testing
  - conventions
related:
  - adr-t1453-2
  - development-testing
  - development-task-registration
  - plan-local-checks-history
  - adr-t1420-1
  - adr-t1707-1
  - adr-t1480-1
  - adr-t2182-1
  - adr-t2588-1
  - adr-t2589-1
  - development-documents
---

# このrepositoryの手元の検証

変更の後に手元で流す検証と、receiptの前の受け入れ条件の対応づけ、askにせず自分で決めるものの今の規則。読むのは、workerが変更を終えてreceiptを書く前（runtimeのworkerのpromptが読ませるAGENTS.mdの「### worker」から辿る）と、人がdagqを通さずcheckoutで直接変えた後。testの書き方と置き場所は[testの制約](testing.md)、文書とcommitの規則とreceiptの前の文書の照合は[文書の規則](documents.md)、taskのverifyの選び方は[taskの登録](task-registration.md)、経緯は[手元の検証とtestの規則の経緯](../plans/local-checks-history.md)とADRが持つ。

## 人の手元の検証

dagqを通さない手元の作業（人がcheckoutで直接変えるとき）は、この3本を通す。

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

## workerの手元の検証

dagqのworkerは全体の`cargo test --locked`を流さず、`cargo llvm-cov`（`cargo llvm-cov nextest`を含む）も手元で流さない。taskのverifyに含まれていても同じ。workerが手元で流すのは次のとおり。

- `sh scripts/cargo-brief.sh cargo fmt --all --check`と`sh scripts/cargo-brief.sh cargo clippy --locked --all-targets -- -D warnings`
  fmt・clippyの関門は着地の検証で、workerが手元で流したかは確かめない。receiptの`tests`のevidenceにその実行を書くことは求めず、reviewも確かめない（[ADR-t2588-1](../adr/2026-10-17-t2588-1-worker-local-fmt-and-clippy-are-not-receipt-evidence-and-not-reviewed.md)）。
- `src/`を変えたら`sh scripts/check-layer-deps.sh`（レイヤーの禁止依存）と`sh scripts/check-context-boundaries.sh`（contextの境界と所有の一覧の食い違い）。CIも実行する。規則と一覧の置き場は[Architecture](../design/architecture.md)の「検査の範囲」
- 変更に関係するtestだけの`cargo test`（次の「testの範囲」と「全体を比べるtest」）
- taskのverifyのうちcoverageの関門（`cargo llvm-cov nextest`と旧い形の`cargo llvm-cov`）と全体の`cargo test --locked`以外（`cargo test --locked --test plugin`など）
- 足した・変えたtestのstress（下の「stress」）
- `tests/it`のtestを足した・変えたら、そのstressのnextestの出力に時間の関門のscript（下の「itのtestの時間の関門」）

containerで動くworker（resumeを含む）の手元のtestはLinuxでbuildされ、macOSに固有のtestは流れない（[testの制約](testing.md)の「macOSに固有のtest」）。
それを変えたときは、流せなかったことをreceiptの`tests`のevidenceに書き、確かめはmainのCIの`checks`に任せる。

着地の関門は`integrate`の検証がrebase後に1回だけ流す。runtimeのtaskではverifyのcoverageの関門を`dagq.toml`の`[landing_verification]`がunit test全件と影響範囲で絞ったIT（既に落ちているtestを除く）に置き換え、llvm-covを含めないtaskではverifyにあれば`cargo test --locked`を流す（[testの制約](testing.md)の「test binary」、仕組みは[integrate](../design/supervisor-lifecycle/integrate.md)と[Validation](../design/supervisor-lifecycle/validation.md#着地の検証)）。全部のtestとcoverageの80%はmainのCIが最終関門として見る。
`dagq.toml`がverificationをcontainerにしたときは、着地の検証はLinuxのcontainerで流れ、macOSに固有のtestはmainのCIの`checks`だけが流す（[ADR-t2182-1](../adr/2026-10-14-t2182-1-macos-specific-tests-are-left-to-the-ci-macos-job-not-the-container-verification.md)）。着地の検証が軽くなっても、workerの手元の検証は上のとおりで変わらない（[ADR-t1925-1](../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)決定6）。workerのpromptがverification_commandsを`integrate`が流すものとして見せ、手元の検証をこの文書に委ねる仕組みは[prompt](../design/supervisor-lifecycle/prompt.md#workerのprompt)の「workerのprompt」のverification commandsの項が持つ。例外は「resumeでの再現」の1つだけ。

buildは原則としてworktreeの既定の`target/`で行う。例外として、worktreeの外にtargetが要るとき（使い捨てのrepositoryのbuildなど）だけ`CARGO_TARGET_DIR`を`$TMPDIR`の下に向け、`/tmp`や`/private/tmp`に直接置かない（Codexのworkerにruntimeが渡す`$TMPDIR`はrunごとに作られ、runの後に消える）。

subagent reviewは該当するときに実行し、しないときは理由をreceiptに書く。

### cargoの出力

workerは手元のcargo（`cargo test`・`cargo nextest run`・`cargo clippy`・`cargo fmt --check`）とそれを流すscriptを`sh scripts/cargo-brief.sh <コマンド>`の形で流す。scriptはコマンドをそのまま実行して出力の全文を`$TMPDIR`の下のログ（`--log <file>`で名指せる）に残し、端末には失敗したtestの名前と出力・コンパイルのエラーとclippyの警告・fmtの差分・要約の行・ログのpathだけを上限つきで出し、元のコマンドの終了コードを返す（何を出すかと上限はscriptの冒頭）。成功の行とcompileの進捗をworkerのcontextに入れないため。

- 失敗の原因を追うなどで全文が要るときは、ログを丸ごと読まず`rg -n '<testの名前やerror>' <ログ>`・`tail -n 50 <ログ>`のように狭く読む。
- 表示が上限で省かれたときも、省いた行数とログのpathが出るので、同じくログを狭く読む。
- 人の手元の検証（上の節）はこの形にしない。

### 実行の記録とreceiptのevidence

workerのturnには、runtimeが手元の検証の記録のdirをenvで渡す。
`scripts/cargo-brief.sh`は流したコマンドを、`scripts/check-it-test-time.sh`は自分の実行を、そのdirに1回ごとに記録する（コマンド・exit status・その時点の作業treeのtree hash・ログなど。stressと時間の関門には種類の名前を付ける。形はscriptの冒頭）。
runtimeはreceiptのcommitのtreeと一致する記録だけをreviewの資料に載せ、`dagq.toml`の`[local_checks]`の宣言と差分から、下の「stress」と「itのtestの時間の関門」が要るか（変えたtestが無ければ要らない）を決めて資料に載せる（[ADR-t2589-1](../adr/2026-10-17-t2589-1-local-check-runs-are-recorded-by-project-scripts-and-matched-to-the-receipt-tree.md)、仕組みは[review](../design/supervisor-lifecycle/review.md)）。

- 記録と判定で示せること（流したコマンド・exit status・stressの周回と結果・時間の関門の結果・変えたtestが無いので関門を省いたこと）は、receiptの`tests`のevidenceに書かない。
- evidenceには記録で示せないことだけを書く: testの範囲を選んだ理由と着地の検証・CIに任せた範囲、流したtestの件数（記録の項目に無く、ログの本文はreviewに渡らない）、流せなかった理由、許可の一覧に足した項目の名前と理由。
- 検証はcommitの後に流すか、木を止めて流してそのtreeのままcommitする。commitの後に1行でも直せば、記録はそのcommitの根拠にならないので流し直す。1回の実行の間に木を変えた記録も根拠にならない。
- 記録はworkerが書けるadvisoryなもので、着地の安全は今どおり`integrate`の検証が持つ。workerが手元でstressと時間の関門を流す規則（[ADR-t920-1](../adr/2026-09-28-t920-1-light-worker-stress-and-heavy-repetition-in-scheduled-ci.md)、[ADR-t1707-1](../adr/2026-10-05-t1707-1-time-gate-for-added-or-changed-integration-tests.md)）は変わらない。

## testの範囲

- 変えた・関係するmoduleを1つずつ`<module>::`で名指しして流す。例: `sh scripts/cargo-brief.sh cargo test --locked --test it <変えた・関係するtestファイルの名前>::`（`tests/it/runtime_claim.rs`なら`sh scripts/cargo-brief.sh cargo test --locked --test it runtime_claim::`。e2eとplugin以外のintegration testは1つのtest binary `it`のmoduleで、ファイル名がmodule名になる。[ADR-0078](../adr/0078-one-integration-test-binary.md)）、`sh scripts/cargo-brief.sh cargo test --locked --lib <module>`。
- 選び方の目安: 変えた`src/`のmoduleのunit test（`--lib <moduleのパス>`）と、その機能の`tests/it`のmodule（例: `tests/it/runtime_claim.rs`を変えたら`runtime_claim::`、`tests/it/lifecycle_replace.rs`なら`lifecycle_replace::`）。
- filterはtestの名前（`<module>::<test>`）の部分一致なので、`runtime_`・`lifecycle_`のような接頭辞だけのfilterは多くのmoduleを選ぶ。`--test it`をfilterなしで流さない、接頭辞だけのfilterや多数のmoduleの列挙で`it`の大半を流さない、`--test it --test plugin`を合わせて全体を流さない。
- `tests/common`や`tests/it/runtime_support`のhelperを変えたら、それを使っているmoduleを流す。使うmoduleが多いときは代表のmodule（数個）に絞り、残りは着地の検証（絞ったITは`tests/common`に触れれば全部のITを流す）とCIに任せたことをreceiptの`tests`のevidenceに書く。
- これは流す範囲の選び方の手がかりで、全体の`cargo test --locked`と`cargo llvm-cov`をworkerが流さない規則は変わらない。
- receiptの`tests`のevidenceには、範囲を選んだ理由（着地の検証とCIに任せた範囲を含む）と流したtestの件数を書く。流したコマンドとexit statusと結果は記録が示すので書かない（上の「実行の記録とreceiptのevidence」）。

## 全体を比べるtest

変えた機能のmoduleに加えて、queueや出力の全体を比べる既存のtestも流す（変えた機能と関係すると気づきにくく、着地の検証の絞ったITが選ばなければCIで初めて落ちやすいため）。

- migrationを足す・変える → `--test it`の`queue_migration::`・`queue_schema::`・`lifecycle_replace::`・`cli_version::`（migrationの前後で行とindexとtriggerを保つこと、別のschemaのqueueを開くこと、互換のmigrationの適用と非互換の拒否、`migrate --check`と`doctor`のschemaを比べる）
- `doctor` / `status`の出力に欄を足す・変える → `--test it`の`cli_tasks::`（`reads_do_not_create_a_queue_and_unknown_tasks_fail`が出力を丸ごとのobjectと比べる）と`cli_read::`
- pluginのskill・referenceの文書を変える → `cargo test --locked --test plugin`（`SKILL.md`の8 KiBの上限と参照を検査する）

## resumeでの再現

- 手元で`cargo llvm-cov`（`cargo llvm-cov nextest`を含む）や全体のtestを流してよいのは、`integrate`の検証が落ちてresumeされたrunが、その落ちたコマンドを手元で流して再現するときだけ。coverageの関門が置き換わったtaskで再現するのは、置き換えの後の着地の検証のコマンド（`sh scripts/landing-it.sh`。`DAGQ_LANDING_BASE`にrebase先のmainのcommitを渡さなければITを全部流し、`DAGQ_CI_KNOWN_FAILURES`を渡さなければ既に落ちているtestも流れて落ちる）で、llvm-covではない（ADR-t1925-1決定6）。再現のコマンドも「cargoの出力」の形で流し（`sh scripts/cargo-brief.sh sh scripts/landing-it.sh`など）、落ちたtestの出力はログを狭く読む。
- rebaseの衝突・migrationの番号・receiptの催促など、検証の失敗以外の理由のresumeと、reviewの差し戻し（revise）はこの例外に当たらず、上の関係するtestだけを流す。

## e2eを流さない

- e2e（`tests/e2e.rs`）はworkerが流さない（[ADR-t1233-2](../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)）。providerに依らず（Codex workerも）同じで、e2eの除外の規則は無い（決定6）。receiptの`e2e`は理由（要るrunはruntimeがreviewのpassの後にhostで流す）つきの`not_applicable`にする。
- 要るrunのe2eは、reviewのpassの後、着地の前にruntimeがhostで全部流す（[着地の前のe2e](../design/supervisor-lifecycle/integrate.md#着地の前のe2e)）。全部のe2eは自動更新と`install`が固定バイナリを入れ替える前の関門も流す（[Auto-update](../design/supervisor-lifecycle/auto-update.md)）。
- 例外: runtimeのe2eが落ちて`needs_session`（`e2e_failed`）でresumeされたrun（理由に落ちたtestとlogがある）は、logを読んで落ちたtestを直してcommitし、再現は落ちたtestを名前で絞って1本ずつ（`sh scripts/cargo-brief.sh cargo test --locked --test e2e -- --ignored --exact <testの名前>`）流すだけにする。全体のe2eは流さない（直したrunはvalidating・reviewの後にruntimeがもう一度流す）。e2eはstressの対象外。

## stress

足した・変えたtestを負荷の下で繰り返す。あからさまに不安定なtest（数周で落ちるもの）を着地前に止める軽い見張りで、稀にしか落ちない不安定さを引き出す重い繰り返しはGitHub Actionsの定時実行が行う（[ADR-t920-1](../adr/2026-09-28-t920-1-light-worker-stress-and-heavy-repetition-in-scheduled-ci.md)、定時実行の規則は[testの制約](testing.md)の「CI」、範囲とissueは`.github/workflows/stress.yml`の冒頭）。

- 対象はrunのdiff（base commitからの差分）で追加・変更した`#[test]`の関数。integration testは`tests/it/**`（`--test it`）、unit testは`src/`の`#[cfg(test)]`（`--lib`）。e2e（`tests/e2e.rs`。`#[ignore]`）とplugin（`tests/plugin.rs`）は対象外。
- コマンドは`sh scripts/cargo-brief.sh cargo nextest run --locked --test it --stress-count 5 -E 'test(=<module>::<name>) | test(=<module>::<name>)'`。unit testは`--test it`の代わりに`--lib`で、名前は`<moduleのパス>::tests::<name>`のようなnextestのtest名。種類ごとに分けて流す。
- 1周が15秒を超えるtestを含むときは`--stress-count 5`の代わりに`--stress-duration 60s`にして上限を付ける。
- 他のrunがhostを使っている負荷の下で流すことに意味があるので、負荷が下がるのを待ってから流さない。
- 1回でも落ちたら、流し直して通ったことで済ませず、原因（固定の時間の待ち、他のtestとの状態の共有、順序への依存など）を直してからもう一度同じstressを通し、receiptを書く。
- 流したこと・周回・結果は記録が示すので、receiptの`tests`のevidenceに書かない（上の「実行の記録とreceiptのevidence」）。
- 変えたtestが無いrun（docsだけ、testに触らない`src/`の変更など）はstressをしない。要らないことはruntimeが差分から判定してreviewに渡すので、evidenceにも書かない。runtimeが要るとしたのに流さなかったときは、その理由をevidenceに書く。
- cargo-nextestはhostに入っている前提（[ADR-t1925-1](../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)決定8。入れ方は[運用](operations.md)の「hostのツール」）で、無ければworkerは入れず、stressをしなかったことと理由（cargo-nextestが無い）をreceiptに書く。
- これは足した・変えたtestだけをworkerの手元で流すもので、全体の`cargo test --locked`と`cargo llvm-cov`をworkerが流さない規則はそのまま。

## 負荷の下で落ちるtestの再現

負荷の下で落ちるtestを直すとき（resumeでの検証の失敗、flaky_testのtaskなど）、workerは再現のためにhostに負荷を足さない（[ADR-t1480-1](../adr/2026-10-05-t1480-1-workers-add-no-load-to-the-host-to-reproduce-failures-under-load.md)）。hostはworker・`integrate`の検証・supervisorの見張りが共有していて、1本のrunが作った負荷が他のrunの検証と見張りを壊すため。

- 起動しないもの: testと別に負荷だけを作るprocess（`yes`・busy loop・stressの道具など）、同時に2本以上の`cargo test` / `cargo nextest`のprocess（backgroundに置いたものも数える）、`[run.env]`が渡すtestの並列度（`NEXTEST_TEST_THREADS`・`RUST_TEST_THREADS`。今の値は`dagq.toml`）より大きい`-j` / `--test-threads`。
- 再現と原因の確かめ方: 記録（eventのdump・log）を読む、待っている条件と上限を確かめる、testの中で遅れを決定的に作る（stubの遅延、上限を縮めるなど）、1本のprocessでの上限つきの繰り返し。
- 上限の数値: 同時に流すtestのprocessは1本、`-j` / `--test-threads`はnextestなら`NEXTEST_TEST_THREADS`、`cargo test`なら`RUST_TEST_THREADS`以下（指定しなければ`[run.env]`の値のまま）、1回の繰り返しは20周か5分（`--stress-count 20`か`--stress-duration 5m`）まで。上の「stress」の5周・60秒より大きいのは原因を確かめる繰り返しだからで、ADR-t920-1が定時実行に移した重い繰り返し（20周以上を高い並列度で同時に複数本）とは、並列度を`[run.env]`の値に留めて1本ずつ流し、時間に上限を付ける点で分けた。繰り返しを何度か流すときも続けて1本ずつ流す。
- それで再現しない稀な失敗は、直せる範囲（待っている条件・上限・順序への依存）を直し、失敗のときの出力（待っていた条件・最後の状態・eventの要約）を増やし、receiptの`follow_ups`（`flaky_test`、`<module>::<name>`で名指す）とCIの定時実行（[testの制約](testing.md)の「CI」）に任せる。receiptの`summary`には再現を試したやり方と、再現しなかったことを書く。
- 上の「stress」との関係: stressは足した・変えたtestを他のrunが作る自然な負荷の下で軽く繰り返すもので、負荷を足さず、負荷が下がるのを待たない。この節はstressを変えず、stressも同じく負荷を作る処理と同時の複数のprocessを使わない。
- 例外: 人がplan reviewのapprove_planのaskで認めた、高い負荷の下での確認をacceptanceに持つtask（ask 323のtask 1360・1361、ask 304のtask 1344）のworkerは、そのacceptanceどおりに確かめてよい。acceptanceが求める範囲（周回・並列度・同時の本数）を超えて負荷を足さない。それ以外のtaskのacceptanceが高い負荷の下での再現を求めていても例外にはならないので、この節のやり方で確かめ、求めに沿えない項目は「askにしないもの」とworkerのpromptのaskの規則どおり`worker_question`（`--because scope`）か`failed`のreceiptにする（例外を認めるのは人だけ。[taskの登録](task-registration.md)の「plan reviewが当てはめる規則」）。

## itのtestの時間の関門

足した・本文を変えた`tests/it`のtestの時間を閾値と比べる（規則は[testの制約](testing.md)の「判断と境界のtest」、値と書式は`scripts/check-it-test-time.sh`の冒頭のコメント）。

- 流すtestを増やさない。上の「stress」で足した・変えたtestを流したnextestの全文のログ（`sh scripts/cargo-brief.sh`が残したもの）にscriptを当てる。端末の短い表示には各testの時間が無いので当てない。`--base`はrunのbase commit（workerのpromptのbase、またはmainとのmerge base）。

  ```sh
  sh scripts/cargo-brief.sh --log "$TMPDIR/it-stress.log" cargo nextest run --locked --test it --stress-count 5 -E 'test(=<module>::<name>)'
  sh scripts/check-it-test-time.sh --base <base commit> "$TMPDIR/it-stress.log"
  ```

- exit 1なら、名指されたtestを直すか、testing.mdの条件に当たる理由で許可の一覧に項目を足し、もう一度当てる。どちらもできなければ`failed`のreceiptに理由を書く。
- 当てたこととexit statusは記録が示すので、receiptの`tests`のevidenceに書かない。許可の一覧に項目を足したら、その名前と理由をevidenceに書く。
- `tests/it`のtestを変えていないrunはscriptを当てず、evidenceにも書かない（要らないことはruntimeが判定する）。stressをしなかったrun（cargo-nextestが無いなど）もscriptを当てず、その理由をevidenceに書く。

## hostに触らない

- resumeされたrunで着地の検証（`cargo llvm-cov nextest`か、それを置き換えた`sh scripts/landing-it.sh`）がcargo-nextestが見つからずに落ちていたら（`no such command: nextest`など）、workerはhostにツールを入れられないので直そうとせず、`failed`のreceiptに「cargo-nextestが無い」と理由を書いて返す（ADR-t1925-1決定8）。`dagq ask`にはしない（[ADR-0047](../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定41の人が要る理由に当たらず、`failed`のreceiptは復旧jobを経て人に届く）。

## 受け入れ条件の対応づけ

receiptの前に受け入れ条件の各項目を根拠へ対応づける（[ADR-t1420-1](../adr/2026-10-03-t1420-1-worker-maps-each-acceptance-criterion-before-the-receipt.md)。仕組みは[prompt](../design/supervisor-lifecycle/prompt.md)の「受け入れ条件の対応づけ」）。満たせない項目をfollow_upに回して`succeeded`にせず、下の「askにしないもの」とworkerのpromptのaskの規則どおり`worker_question`か`failed`のreceiptにする。このrepositoryでの根拠の書き方:

- testは`<module>::<test>`の名前と流したコマンド（流す範囲は上の「testの範囲」の規則のまま）、文書はpathと節、測定は文書の節・CSVとscriptのpath・コマンドと`--since` / `--until`の区切り。
- baseがすでに全ての条件を満たし差分が要らないときは、commitせずにreceiptの`result`を`already_satisfied`にし、`base_checks`にbaseで行った確かめのコマンドと結果を書く（[ADR-t1818-1](../adr/2026-10-05-t1818-1-worker-claims-already-satisfied-and-runtime-closes-on-a-verified-snapshot.md)、形は[Receipt and session exit](../design/supervisor-lifecycle/receipt-and-session-exit.md#baseがすでに満たすときalready_satisfied)）。
  確かめは上の「workerの手元の検証」で許されたものだけで、coverageの関門と全体の`cargo test`は流さない（taskのverifyの全体はruntimeがmainのsnapshotで流す）。
- 測定のtaskでは条件が求める周回数と実際に流した回数を比べ、足りなければ理由とともに`worker_question`か`failed`にし、少ない周回の結果で`succeeded`にしない。
- 条件が「ほかに同じ形のもの」のように範囲を広く書くときは、grepなどで洗い出した一覧と各々の扱い（直した・残す理由）を`summary`に書く。条件が「各testの前後の秒」のように値を項目ごとに求めるときは、合計だけでなく求めた単位で書く。

続けて行う文書の照合は[文書の規則](documents.md)の「workerの文書の照合」。

## askにしないもの

workerが`dagq ask`にしてよいのは、人が要る理由（[ADR-0047](../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定41の`reason_category`。workerでは主に受け入れ条件や範囲が変わる`scope`と、成果を捨てるかどうかの`discard`）に当たるときだけ。当たらないもの（実装の選び方、ADRやmigrationの番号の衝突など）は自分で決めてreceiptの`summary`に書き、taskの範囲の外に出るなら`failed`のreceiptに理由（必要なパスや作業）を書く。ADRの番号の衝突の直し方は[文書の規則](documents.md)の「ADRのID」、migrationは[migrationの規則](migrations.md)の「番号」、cargo-nextestが無いときは上の「hostに触らない」。askの打ち方・`--topic`の分類コード・follow_upsの`category`はworkerのpromptと[ask](../design/supervisor-lifecycle/ask.md#worker_questionの分類コード)・[Receipt and session exit](../design/supervisor-lifecycle/receipt-and-session-exit.md#follow_upsの分類コード)が持つ。
