---
id: development-task-registration
type: development
title: このrepositoryのtaskの登録（verify・paths・evidence・changeの選び方、固定バイナリを待つ宣言、依存の付け方、負荷の下で落ちるtestを直すtask、ADRを書くtask、plan reviewが当てはめる規則）
status: current
created: 2026-10-03
owners:
  - hisamekms
tags:
  - planning
  - conventions
related:
  - adr-t1985-1
  - adr-t1971-1
  - adr-t1639-1
  - adr-t1639-2
  - adr-t1453-2
  - adr-t1504-1
  - adr-t1504-2
  - adr-t1591-1
  - adr-t1480-1
  - development-local-checks
  - development-testing
  - development-migrations
  - plan-local-checks-history
  - development-documents
---

# このrepositoryのtaskの登録

このrepositoryでtaskを`dagq add`するときの`--verify`・`--paths`・`--evidence`・`--change`と、goal の優先度・ラベルの今の選び方。読むのは、taskを登録・修正するplannerと、proposalを見るplan review job（AGENTS.mdの「plan review」から辿る）。登録の汎用の手順（flagの意味、宣言外のpathを変えたrunの扱い、pathsの変え方）はpluginの`dagq`の`reference/scope.md`と`reference/register.md`、workerが手元で流すものは[手元の検証](local-checks.md)、testの規則は[testの制約](testing.md)が持つ。

## 推奨の組み合わせ

変更の対象で検証を選ぶ。globはrepository root起点で、`*`は1階層、`**`は任意の深さ。

- docsだけ: `--paths 'docs/**' --paths '*.md' --verify 'cargo fmt --all --check' --verify 'sh scripts/check-frontmatter-dates.sh' --verify 'sh scripts/check-doc-links.sh' --verify 'sh scripts/check-doc-frontmatter.sh' --verify 'sh scripts/check-design-docs.sh'`（fmtも要らなければ4本のscriptだけ。`check-frontmatter-dates.sh`はfrontmatterの日付の行（[frontmatter仕様](../frontmatter.md)が持たせるもの）にコメントがあれば、`check-doc-links.sh`は`docs/`とrootの`.md`の相対リンクが実在しないpathを指せば、`check-doc-frontmatter.sh`はADR以外の`docs/`の文書のfrontmatterが必須のkey・typeとstatusの値・idの形と重複で[frontmatter仕様](../frontmatter.md)に合わなければ落ちる検査で、どれも数秒で終わりCIも実行する。`check-design-docs.sh`は`docs/design/`の文書ごとの大きさ・1行の長さ・本文のtask番号の上限と許可の一覧を検査し、CIも実行する。上限と許可の一覧の扱いは[文書の規則](documents.md)の「design」）
- ADRを書く（docsだけ）: 上に`--verify 'sh scripts/check-adr-numbers.sh'`を足す（上の`check-design-docs.sh`も残す）
- pluginの文書・skill: `--paths 'plugins/**' --paths 'docs/**' --paths '*.md' --verify 'cargo test --locked --test plugin' --verify 'sh scripts/check-frontmatter-dates.sh' --verify 'sh scripts/check-doc-links.sh' --verify 'sh scripts/check-doc-frontmatter.sh' --verify 'sh scripts/check-design-docs.sh'`（`check-design-docs.sh`は上のdocsだけの行と同じ。`tests/plugin.rs`がskillの大きさと参照を検査するのでtestを残す。pluginの文書を読むtestはこれだけ）
- runtime（`src/`・`tests/`・`migrations/`・`crates/`）: `--paths`なしで`cargo fmt --all --check`・`cargo clippy --locked --all-targets -- -D warnings`・`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`・`sh scripts/check-design-docs.sh`（runtimeのtaskは`docs/design/`も変えうるため。中身は上のdocsだけの行と同じ）。llvm-covの行は着地では`dagq.toml`の`[landing_verification]`がunit test全件と絞ったITに置き換え、coverageはCIが見る（下の「coverageの関門」）。`src/`を変えるなら`sh scripts/check-layer-deps.sh`を足す（レイヤーの禁止依存と許可の一覧の古い項目を検査する。CIも実行する。規則と一覧の書式は[Architecture](../design/architecture.md)の「検査の範囲」）。`--evidence e2e`は一律には付けない（付ける目安は下の「e2e」）
- brokerのcrate（`crates/`の`dagq-broker-protocol`・`dagq-broker`・`dagq-broker-client`）を変える（runtime）: 上のruntimeの組み合わせのまま。着地の検証が全てのcrateのunit testと、対応表と差分で選ぶ`crates/<crate>/tests/`を含む絞ったITを流し（CIの関門は`--workspace`で全てのcrateのtestを流す。[ADR-t828-1](../adr/2026-09-28-t828-1-coverage-gate-covers-the-workspace-with-workspace-flag.md)）、rootの`Cargo.toml`の`default-members`が全てのcrateを入れるので`clippy --all-targets`も覆うので、`cargo test --locked -p <crate>`はverifyに重ねない（workerが手元で流す。[手元の検証](local-checks.md)の「testの範囲」）。podmanを要るtestは`#[ignore]`で関門に数えない（[testの制約](testing.md)）ので、verifyに`--ignored`を足さない。brokerの`[broker]`を本番の`dagq.toml`に足すtaskは登録しない（[運用](operations.md)の「`dagq.toml`を変えるとき」）
- migrationを足す（runtime）: 上のruntimeの組み合わせに`--verify 'sh scripts/check-migration-numbers.sh'`を足す
- e2eの印（`.config/e2e-quarantine.toml`）を変える: verifyに`sh scripts/check-e2e-quarantine.sh`を付ける（書式・重複・testの実在・上限を検査し、期限切れは警告だけ。CIも実行する。印の規則は[testの制約](testing.md)の「e2eの印」）
- pluginとバイナリのversionを変える: verifyに`sh scripts/check-plugin-version.sh`を付ける（検査の中身は[plugin integration](../design/plugin-integration.md)の「tagとversionの一致規則」）
- itのtestの時間の関門の許可の一覧（`.config/it-slow-allow.toml`）か関門のscript（`scripts/check-it-test-time.sh`）を変える: verifyに`sh scripts/check-it-test-time.sh --self-test`を付ける（fixtureでscriptの照合を確かめ、このrepositoryの許可の一覧の書式を読む。関門そのものはCIが流す。許可の一覧に載せてよい理由は[testの制約](testing.md)の「判断と境界のtest」、書式は[Slow tests](../design/slow-tests.md)の「itのtestの時間の関門」）
- AGENTS.mdを変える: verifyに`sh scripts/check-agents-md-size.sh`を付ける（byteの上限を検査する。CIも実行する。上限と、規則の本文をAGENTS.mdに足さないことは[文書の規則](documents.md)の「AGENTS.md」）
- 形式の検査のscript（`scripts/check-*.sh`）を足す・変える: verifyにそのscriptと`sh scripts/check-scripts-root.sh`を付ける。scriptは検査するtreeのrootをcwdのgitのtop（`git rev-parse --show-toplevel`）から取り、cwdがgitのwork treeの外のときだけscriptの場所から求める（runのプログラムのreviewはlanding branchのscriptを一時の場所に書き出し、cwdをrunのworktreeにして流すので、scriptの場所から求めるとreviewの対象のtreeを見ない）。`check-scripts-root.sh`の`scripts`の一覧にscriptを、`cases`にそのscriptの違反の場合と違反を置く`violate_<case>`を足す（HEADの一時のcloneにだけ違反を置き、写したscriptがexit 1・元のrepositoryではexit 0になることを確かめる。CIも実行する）。プログラムのreviewに挙げるscriptは、この一覧に入ってこの約束を満たすものだけにする
- configだけ（設定と運用）: 変えるものだけを`--paths`に挙げる（`--paths 'dagq.toml'`・`--paths 'scripts/**'`・`--paths '.github/**'`・`--paths '.config/**'`・`--paths '.dagq/**'`・`--paths 'rust-toolchain.toml'`から選ぶ。runtimeのpathは含めない。下の「plan reviewが当てはめる規則」）。verifyは変えるものの検査: `dagq.toml`なら読めることを確かめる`--verify '/Users/shinnosukeooyama/.local/share/mise/installs/python/3.12/bin/python3 -c "import tomllib; tomllib.load(open(\"dagq.toml\", \"rb\"))"'`（task 942・1593の先例と同じく、Python 3.11以上のpythonを絶対pathで名指す。構文だけを見てkeyの意味は見ない。integrateのhostの素の`python3`は3.9.6で`tomllib`が無く着地に失敗するので使わない。[ADR-t883-1](../adr/2026-09-30-t883-1-edit-ended-run-verification-before-inherited-retry.md)のContext）、scriptと`.config/`の印・一覧はこの節のそれぞれの行の検査（`check-e2e-quarantine.sh`・`check-it-test-time.sh --self-test`など）。検査の無いもの（CIのworkflowなど）は検証なし。新しいkeyを足すのは本番の固定バイナリが読めるようになってから（[運用](operations.md)の「`dagq.toml`を変えるとき」）
- `docs/`以下の`.md`を変えるtask（`--paths`に`docs/**`を含むtaskと、`--paths`なしのruntimeのtaskで`docs/design/`などを変えるもの）は、runtime・plugin・configのtaskでも、対象の行のverifyに加えて`--verify 'sh scripts/check-frontmatter-dates.sh'`・`--verify 'sh scripts/check-doc-links.sh'`・`--verify 'sh scripts/check-doc-frontmatter.sh'`を付ける。rootの`.md`（AGENTS.mdなど）だけを変えるtaskは`--verify 'sh scripts/check-doc-links.sh'`を付ける（着地の検証はtaskのverify（coverageの関門の置き換えを除く）だけなので、docsの行を取らないtaskでも日付の行のコメント・リンク切れ・frontmatterの欠けをpushの前に止めるため）。
- 対象が混ざるtaskは重い方の検証にする（`docs/`を変えるなら上の行の3本のscriptも足す）。

llvm-covとcargo testの重ね方:

- llvm-covをverificationに含めるtaskでは`cargo test --locked`をverificationに重ねない。着地ではllvm-covの行がunit test全件と絞ったITに置き換わり（[testの制約](testing.md)の「test binary」）、`cargo test --locked`を並べると置き換えの外でITを全部流し直すので、着地の検証を軽くした分が戻る。
- llvm-covを含めないtask（docs・pluginの文書など）は、必要なら`cargo test --locked`をverificationに残す。

## 固定バイナリがtask Xを含んでから行うtask

固定バイナリがtask Xの着地を含むことを要するtask（新しいflagの登録案内や、新しいkeyを使うconfigなど）は、`dagq add ... --wait-for-build --depends-on X`で登録する。必要な依存先が複数なら`--depends-on`を繰り返す。宣言は`wait_for_build`で、supervisorは自分のbuild識別子のcommitが直接の依存先の着地commitを全て含むまでclaimしない（[claimを控える](../design/supervisor-lifecycle/claim-defer.md#依存先の着地を含むbuildを待つtask)、[Domain model](../design/domain-model.md)）。draft / submittedのtaskは`edit TASK --wait-for-build`で宣言し、`--no-wait-for-build`で外せる。

verifyの1本目に、`~/.local/bin/dagq --version`のbuild識別子と依存先の着地を`git merge-base --is-ancestor`で比べる手書きのscriptを置かない。verifyはclaimの後にしか効かないため、早すぎるclaimを止めるのはこの宣言と`--depends-on`にする。自動更新はruntimeを変える着地でだけbuildするので、runtimeを変えない依存先だけを待つtaskには宣言しない（[ADR-t1632-1](../adr/2026-10-05-t1632-1-claim-waits-for-a-build-that-contains-the-dependencies-landings.md)）。

## pathsと軽い検証

変更の対象でverificationを軽くしてよい。条件は`--paths`で変えてよいパスを宣言すること（[ADR-0029](../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）。宣言外のパスを変えたrunはvalidatingで`needs_session`（`scope_violation`）になり、`integrate`もrebase後の差分を同じく検査して着地させないので、軽い検証のまま`src/`の変更が入ることはない。`--paths`を付けないtaskは制限されない。宣言外のパスが本当に要るときのworkerとplannerの手順はpluginの`dagq`の`reference/scope.md`の「What happens outside the paths」「Change the paths」が持つ。

## runtimeのtaskの主なファイル

runtimeのtaskは`--paths`を宣言しない（上の「推奨の組み合わせ」）ので、主に触るファイルをdescriptionに書く（例: 「主に`src/application/supervise/plan_review.rs`と`tests/it/plan_review.rs`を触る」）。読み手（plan reviewの衝突の検出と`related`）、予想で制限でないこと、`--paths`に書かない理由はpluginの`dagq`の`reference/scope.md`の「Name the files a task without paths mainly touches」が持つ。

## coverageの関門

- 行カバレッジの80%の関門はCIだけが見る（[ADR-t1925-1](../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)決定1・5）。着地では、`dagq.toml`の`[landing_verification]`がtaskのverifyのcoverageの関門の行を、unit test全件と影響範囲で絞ったIT（既に落ちているtestを除く）の1本のコマンドに置き換え、他の行は登録のまま流す（決定4。仕組みは[Validation](../design/supervisor-lifecycle/validation.md#着地の検証)）。coverageを下回る変更も着地し、CIの見張りが修正taskにする。
- 新しく登録するruntimeのtaskも、verifyには上の「推奨の組み合わせ」のとおりcoverageの関門（`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`）を書く。この行が着地の検証に置き換わる印で、無いtaskは着地でunit testもITも流れない。`sh scripts/landing-it.sh`をverifyに直接書かない（`[landing_verification]`を外したときに登録のコマンドに戻せなくなる）。
- 登録済みのtaskの`cargo llvm-cov nextest --locked --fail-under-lines 80`と`cargo llvm-cov --locked --fail-under-lines 80`は書き換えない。どちらの形も着地では同じく置き換わる（決定4）。
- 着地の検証もCIもcargo-nextestで流すので、cargo-nextestが無いhost（[運用](operations.md)の「hostのツール」）では着地の検証が失敗する。旧コマンドで登録しても同じく置き換わるので避けられず、人がhostに入れる。

## e2e

- runtimeのtaskに`--evidence e2e`を一律には付けない。e2eの要否はvalidatingがrunの差分と`dagq.toml`の`[e2e] paths`から決めて`validation_finished`の`e2e_requirement`に記録し、要るrunにはreviewのpassの後にruntimeがhostでe2eを流す（[ADR-t963-1](../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定2・3、[ADR-t1233-2](../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)。仕組みは[Validation](../design/supervisor-lifecycle/validation.md)の「runtimeが流すe2e」と[Review](../design/supervisor-lifecycle/review.md)の「着地の前のe2e」。receiptの`e2e`のevidenceを求めないことも同じ）。
- `--evidence e2e`を明示したtaskは差分に依らずe2eが要るので、`[e2e] paths`の外でも実cmuxで確かめる必要があるとplannerが判断したときだけ付け、理由をdescriptionに書く。
- その判断の目安: runをまたぐsupervisorの振る舞い（superviseの回ごとのclaim・landing recheck・resume・`integrate --next`の着地の順・superviseの回ごとのrunの本数）を変え、`tests/e2e.rs`の複数passの筋書きを変えうるtaskには付ける。これらを変える差分は主に`[e2e] paths`の外のファイル（superviseの判断の部分（`src/application/supervise/`の`handoff.rs`と`update.rs`以外）や、claimと着地の順を決めるstoreとdomain）に収まるので、付けないと着地の前のe2eが流れない。task 1310（landing recheck）はこの外で着地の前のe2eが流れずに着地し、自動更新の関門で`tests/e2e.rs`の`two_independent_tasks_run_concurrently_and_a_dependent_follows_integration`が落ちて固定バイナリの更新が止まった（ask 350。testはtask 1516が直した）。`[e2e] paths`にsuperviseの判断のファイルを足して防がないのは、runtimeのcommitの多くが触るので狭める効果がほぼ消えるため（`dagq.toml`のコメントとtask 1516の判断）。目安に当たらないsuperviseの変更（1つのrunの中の判断だけを変えるもの）には付けない。

## change

- `--change`はtaskが行う変更の種類を1つ宣言する（[ADR-t980-1](../adr/2026-09-29-t980-1-classify-runs-by-declared-change-and-diff-derived-area.md)）。値の集合は`dagq.toml`の`[tasks] changes`で、`add`のたびに付ける。集合の外の値とchangeの無いtaskを拒む仕組みはpluginの`dagq`の`reference/register.md`の「Task fields」（`--change`の箇条）、areaを着地の差分から求める仕組みは同じskillの`reference/scope.md`の「Recommended combinations」、areaの対応表は`dagq.toml`の`[areas]`が持つ。
- このrepositoryの7値の意味:
  - `feature`: 振る舞いや機能を足す・変える。ADRと実装を1 taskにするものも
  - `fix`: 決まった振る舞いと違う不具合を直す。不安定なtestを直すのは`test`
  - `refactor`: 振る舞いを変えずに構造を直す。ファイルの分割・名前の変更・重複の除去
  - `test`: testだけを足す・直す。不安定なtestの修正とtestのhelperを含む
  - `measure`: 数えて確かめる・測る。statsやkpiを読んで結果をdocsに書く測定と、測るための一時の計装
  - `docs`: 文書だけ。ADR・design・pluginのskill・AGENTS.md。実装を伴わない決定の記録
  - `config`: 設定と運用だけ。`dagq.toml`・CI・`scripts/`・toolchain。`Cargo.toml`・`Cargo.lock`を変えるもの（versionと依存）はbuildの設定を変えるのでconfigにせず、`--paths`なしのruntimeの組み合わせで主な目的のchangeにする（下の「plan reviewが当てはめる規則」）
- `dagq.toml`の`[supervisor] light_changes`に置いたchange（このrepositoryではdocsとconfigを置く予定で、置くのはtask 1593）のtaskは、着地の順番を待つだけのrunが空けた枠でもclaimされうるが、そこでclaimされるのは`--paths`を宣言したtaskだけなので、docs・configのtaskには`--paths`の宣言が要る（[ADR-t1591-1](../adr/2026-10-04-t1591-1-landing-queue-leaves-room-for-light-changes.md)決定2・3、判定は[claimを控える](../design/supervisor-lifecycle/claim-hold.md#着地待ちが空けた軽い枠)）。軽い枠に重い変更が紛れると、`parallel`と`[run.env]`が前提にする重いbuildの同時数を超えるため。宣言の外を変えたrunは軽い枠のものも上の「pathsと軽い検証」のとおり止まる。
- 混ざるときは主な目的の1つを選ぶ（例: 不具合の修正にtestを足すなら`fix`、新しい機能のdocsを同じtaskで書くなら`feature`）。changeは検証を決めない（検証は上の推奨の組み合わせのとおり、変更の対象で選ぶ）。
- 作業時間の前後比較で読む層は[運用](operations.md)の「KPIの読み方と印」。

## goal の優先度とラベル

この repository のラベルの語彙は `dagq.toml` の `[goals] tags` が正本で、[棚卸しの案](../plans/goal-priority-inventory.md#1-ラベルの語彙の案)（task 1642）の 11 語を採用する。ラベルはテーマを表し、優先度は表さない。goal の主題に合うものを 1 つ以上付け、主なラベルを先頭に置く。複数のテーマに関わるときは該当するものを添える。変更するファイルの種類は `[areas]`、task の変更の種類は `change` が持つので、それだけを理由にラベルを選ばない。

| ラベル | 意味 | 付ける基準 |
|---|---|---|
| `headless` | worker・planner・job を非対話（turn ごとの呼び出し）と background の wrapper で動かすこと | 非対話の経路・wrapper・その評価と測定を作るか変える goal。非対話の run の stall や待ちの改善が主題なら `reliability` を主にして `headless` を添える |
| `codex` | Codex の provider（worker・job・planner・inbox）を使えるようにし、Claude と比べること | provider を Codex に広げるか切り替える goal。レビュー系かどうかでラベルを分けず、優先度は goal ごとに判断する |
| `cmux` | cmux への依存（backend の呼び出し・対話の経路・workspace・it と e2e の cmux の fake）を減らすか安定させること | cmux を呼ぶ箇所を消すか、その呼び出しの失敗を扱う goal と、その案内の整理 |
| `throughput` | 着地の速度・着地の検証と test の時間・slot と claim・CPU の取り合い | 着地までの時間か着地の件数を直接動かす goal。数えるだけのものは `observability` |
| `enterprise` | 他の repository・隔離環境・認可・Linux とコンテナ・配布とリリースで dagq を使えるようにすること | dogfooding の前提を取り除く goal |
| `planning` | planner・plan review・goal・follow-up の所属・依頼（request）・Spike・再計画の流れ | 計画を立てる・検査する・goal を開閉する仕組みを変える goal |
| `observability` | 計測・stats・kpi・日次と毎時の見直し・finding・トークンの記録 | 数えて読めるようにすることが主題の goal |
| `architecture` | レイヤーとコンテキストの責務の分割・境界の検査・refactor | 振る舞いを変えずに構造を変える goal |
| `reliability` | supervisor・自動更新・復旧 job・inbox の届け・host の後片付けが止まらず残さないこと | 運用の停止・取りこぼし・資源の漏れを直す goal |
| `review` | run の review・plan review・goal review の判定とその差し戻しの減らし方 | review 系の actor の判定や入力を変える goal（provider の切り替えは `codex`） |
| `docs` | 文書と skill の整合（link・AGENTS.md の大きさ・リリースの文） | 文書だけを直す goal |

`test`・`security`・`plugin` は語彙に足さない。test の時間は `throughput`、隔離と後片付けは `reliability`、認可と隔離環境は `enterprise` を使い、plugin というファイルの範囲は `[areas]` で表す。

goal の優先度を正本にし、task は個別の指定が無ければ所属の goal から継ぐ（[ADR-t1639-1](../adr/2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)）。同じ goal の task を個別の指定で一括して揃えず、goal の優先度で決める。この repository で段を選ぶ目安は次のとおり。人の明示した優先順と、goal 全体を待たせたときの影響を根拠に選ぶ。

| 段 | この repository での目安 |
|---|---|
| `interrupt` | 通常の順に待たせられず、人が割り込みを必要とする例外。非対話化や Codex 化というテーマだけで選ばず、常用しない |
| `urgent` | supervisor の停止など、運用を止めている不具合の解消 |
| `high` | 他の開発を進める前提や、効果の大きい着地・検証のスループットの改善。効果と先に要る理由を書く |
| `normal` | 通常の機能開発・整理。エンタープライズ対応も、割り込みや先行が要る根拠が無ければこの段を目安にする |
| `low` | 後回しの改善、大きな拡張や条件待ち。テーマごとの受け皿の goal はこの段にする |

この目安は AI が付ける goal（AI 由来の新しい goal と、人の言葉に優先度の無い新しい goal）に当てはめる。人が明示した優先度（`interrupt` を含む）は目安で変えない。人の言葉に優先度の無い新しい goal に目安で段を付けたときは、そのことを goal の description か task の context に書く。AI 由来の task（由来の判定は [ADR-t1971-1](../adr/2026-10-07-t1971-1-plan-review-keeps-human-origin-priority-and-membership-and-ai-tasks-inherit-goal-priority.md) 決定1・2）には個別の優先度を付けず goal から継がせ、優先度を変えたいときは所属か goal の優先度を見直す。

2026-10-02 の段と 2026-10-03 の方針は棚卸しの初期値の案の材料で、テーマと段を恒久的に結び付ける規則ではない。既存の goal ごとの優先度案と今の task の値とのずれは[棚卸しの「2. goal ごとの表」](../plans/goal-priority-inventory.md#2-goal-ごとの表)にあり、値の適用と今の並びとの調整は人が別に決める。この設定と文書の変更では既存の goal・task の値や所属を変えない。

受け皿は元の goal ごとの積み残しではなく、同じラベルのテーマごとにまとめ、優先度を `low` にする（[ADR-t1639-2](../adr/2026-10-04-t1639-2-defer-improvements-outside-acceptance-to-a-low-goal-per-tag.md)）。goal・task の優先度の付け方と継承、後回しの判定・所属の記録・受け皿の選択と移動の汎用の手順は plugin の `dagq` の `reference/register.md` が持つ。

## 依存の付け方

- taskの依存（`--depends-on`）は中身の前提（先に着地しないと作業が成り立たない）に限る。同じファイルの衝突を避けるためだけの依存は付けず、askにもしない。同じファイルの衝突は、衝突の多いファイルならruntimeのclaimの控え（[claimを控える](../design/supervisor-lifecycle/claim-defer.md)。上限の時間まで、`interrupt`のtaskは控えない）が扱い、残りはrebaseか着地の衝突として解く（[ADR-t1985-1](../adr/2026-10-07-t1985-1-dependencies-only-for-content-prerequisites-and-conflicts-left-to-claim-deferral.md)決定1）。
- 依存を付けるときは、noteかtaskのcontextに種類（中身の前提か衝突回避か）と理由を書く（決定2）。
- 人に聞く（`planner_question`、`--because scope`）のは、(a) 中身の前提の依存で、(b) その依存で低い優先度のgoalのtaskが効く優先度を継いで上がり、(c) 順番の変更・taskのgoalへの取り込み・分割などplannerの手で解けない、の全部を満たすときだけにする。問いは「低いgoalのtaskを（優先度ごと）前に出すか、高いgoalが待つか」にする。どれかを満たさなければ自分で決めて進め、理由をnoteかcontextに残す（決定3）。

## 負荷の下で落ちるtestを直すtask

- 負荷の下で落ちるtestを直すtaskのacceptanceとdescriptionに、高い負荷の下での再現を求めない（[ADR-t1480-1](../adr/2026-10-05-t1480-1-workers-add-no-load-to-the-host-to-reproduce-failures-under-load.md)決定(d)）。求めないもの: loadの値の指定（「load average 20以上で」など）、`-j` / `--test-threads`の`[run.env]`の並列度を超える引き上げ（`-j 16`など）、同時の複数のstressかtestのprocess、負荷をかける処理（`yes`・busy loop・stressの道具、「他のrunか負荷をかける処理」）。workerはそれらをhostで起動しないため（[手元の検証](local-checks.md)の「負荷の下で落ちるtestの再現」）。
- 代わりに、再現と確かめ方は記録（eventのdump・log）を読む、待っている条件と上限を確かめる、testの中で遅れを決定的に作る（stubの遅延、上限を縮めるなど）、1本のprocessでの上限つきの繰り返し（上限の数値は同じ節）から選んで書き、それで再現しない稀な失敗は直せる範囲を直して失敗のときの出力を増やし、follow_up（`flaky_test`）とCIの定時実行（[ADR-t920-1](../adr/2026-09-28-t920-1-light-worker-stress-and-heavy-repetition-in-scheduled-ci.md)）に任せる、と書く。
- 例外は、plan reviewのapprove_planのaskに人がreadyと答えて、高い負荷の下での確認をacceptanceに持つtaskを認めたものだけ（ask 323のtask 1360・1361（event 62021・62046に承認と適用）、ask 304のtask 1344）。これらは上の対象外で、workerはacceptanceどおりに確かめてよい（acceptanceの範囲を超えて負荷は足さない）。新しい例外は人のapprove_planの答えでだけ認められ、plannerもplan reviewも自分では例外を作らない（決定(e)）。既存の例外を取り消すときも、人の判断を得てから1360・1361を計画し直す。

## ADRを書くtask

- ADRを書くtaskを登録するときは、descriptionに本数と各IDの中身を書く（自分のIDは`add`が返すまで分からないので「このtaskのIDでADR-t<ID>-1を書く」と書くか、`add`の後にdraftを直す）。verifyは上の「推奨の組み合わせ」の「ADRを書く」の行。IDの形と、番号の割り当ての棚卸しをしないことは[文書の規則](documents.md)の「ADRのID」。
- 番号付きの決定を複数持つADRの一部を変える（`amends`）か、丸ごと置き換えるかを、ADRを書くtaskのplannerがdescriptionに書き、plan reviewが見る（下の「plan reviewが当てはめる規則」）。選び方は[文書の規則](documents.md)の「ADR」。
- 決定と実装が明らかなものは、ADRと実装を1 taskにする（[ADR-t598-1](../adr/2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)決定12）。

## plan reviewが当てはめる規則

この repository のplan review jobは、proposalのtaskに次を当てはめる（読む文書はAGENTS.mdの「plan review」が名指す）。

- 固定バイナリが依存先の着地を含むことを要するtaskは、上の「固定バイナリがtask Xを含んでから行うtask」のとおり`--wait-for-build`の宣言と必要な`--depends-on`を持ち、verifyの手書きの関門で代用していないこと。宣言や依存が欠けている、または手書きの関門で代用していれば`revise`にする。
- verify・paths・evidenceは上の「推奨の組み合わせ」（llvm-covと`cargo test`の重ね方を含む）と「e2e」に合い、changeは上の「change」のとおりtaskの主な目的に合う1つであること。runtimeのtaskに一律の`--evidence e2e`は求めないが、上の「e2e」の目安（runをまたぐsupervisorの振る舞いを変え、`tests/e2e.rs`の複数passの筋書きを変えうる）に当たるtaskに`--evidence e2e`が無ければ`revise`にする。`--evidence e2e`が付いていれば、`[e2e] paths`の外でも実cmuxで確かめる理由（目安に当たることを含む）がdescriptionにあるかを見る。
- changeが`docs`か`config`のtaskは`--paths`を宣言し、そのglobがruntimeのpath（`src/`・`tests/`・`migrations/`・`crates/`と、buildの設定の`Cargo.toml`・`Cargo.lock`・`build.rs`）に当たらないこと。`docs`は上の「推奨の組み合わせ」のdocs・pluginの文書の行のpaths、`config`は同じ節のconfigの行のpathsの範囲に収める。`--paths`が無い・runtimeのpathを含む（`**`のような広いglobを含む）・descriptionやacceptanceがruntimeのpathの変更を求める、といった食い違いは`revise`にし、主な目的に合うchange（`feature`・`fix`・`refactor`など）に直すか`--paths`を絞らせる。上の「change」の軽い枠で重いbuildが走らないようにするため（[ADR-t1591-1](../adr/2026-10-04-t1591-1-landing-queue-leaves-room-for-light-changes.md)決定3。軽い枠でclaimしてよいかの判定はruntimeが持ち、この規則は登録の時にpathsの中身を見る）。
- `docs/`以下の`.md`を変えるtask（`--paths`に`docs/**`を含むか、descriptionやacceptanceが`docs/`の文書の変更を挙げるもの。runtime・plugin・configのtaskを含む）のverifyに`sh scripts/check-frontmatter-dates.sh`・`sh scripts/check-doc-links.sh`・`sh scripts/check-doc-frontmatter.sh`のどれかが無ければ`revise`にする（上の「推奨の組み合わせ」の`docs/`を変えるtaskの行）。
- `docs/design/`を変えうるtask（`--paths`に`docs/**`か`docs/design/`に当たるglobを含むtask、`--paths`なしのruntimeのtask、descriptionやacceptanceが`docs/design/`の文書の変更を挙げるtask）のverifyに`sh scripts/check-design-docs.sh`が無ければ`revise`にする（上の「推奨の組み合わせ」のdocsだけ・ADR・pluginの文書・runtimeの行）。configだけのtaskには求めない。
- 測定のtask（changeが`measure`のtaskと、受け入れ条件に測定を含むtask）は、周回数（と交互に流すか）、表の列、値の計算式（何を何で割るか、待ちを引くときの区間）、証拠の所在（文書の節・CSV・script・コマンドと時刻の区切り）をacceptanceかdescriptionに書くこと（測定の形に当たらない項目、例えば1回だけ読む測定の周回数は、当たらない理由を書く）。条件の範囲を「同じ形のもの」で広げるtaskは、範囲を決めるgrepか一覧を書くこと。欠けていれば`revise`（workerはこれらを根拠に受け入れ条件の各項目を対応づける。[ADR-t1420-1](../adr/2026-10-03-t1420-1-worker-maps-each-acceptance-criterion-before-the-receipt.md)）。
- ADRの索引（`docs/adr/INDEX.md`、無ければ`sh scripts/adr-index.sh`で作る。[文書の規則](documents.md)の「ADR」）と、taskが名指すADRと`docs/design/`の文書を読み、`accepted`のADRの決定と矛盾するtaskは`concern`にする（`superseded`なら`superseded_by`を辿る）。
- ADRを書くtaskが上の「ADRを書くtask」を満たすこと（IDとファイル名の形、`check-adr-numbers.sh`のverify、置き換えか`amends`か）。足りなければ`revise`。
- 挙動や仕様を変えるtaskは、関連文書（[文書の規則](documents.md)の「workerの文書の照合」が挙げる文書）のpath・節と更新が要る理由をdescriptionかcontextに書くこと。欠けていて関連する文書が明らかなら、見つけたpathを理由に書いて`revise`にする。文書の差分を求めるverifyやevidenceは求めない（[ADR-t1428-1](../adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)）。
- 負荷の下で落ちるtestのtaskが上の「負荷の下で落ちるtestを直すtask」を満たすこと。acceptanceかdescriptionが高い負荷の下での再現を求めるtaskは`revise`にする（[ADR-t1480-1](../adr/2026-10-05-t1480-1-workers-add-no-load-to-the-host-to-reproduce-failures-under-load.md)決定(d)）。ただし人がapprove_planのaskにreadyと答えて認めたtask（ask 323のtask 1360・1361、ask 304のtask 1344）は例外として保ち、`revise`にしない。plan reviewは自分で新しい例外を作らない（決定(e)）。
- 優先度と所属はproposalの由来で分けて見る（[ADR-t1971-1](../adr/2026-10-07-t1971-1-plan-review-keeps-human-origin-priority-and-membership-and-ai-tasks-inherit-goal-priority.md)）。人間由来（requestに結ばれたproposal、持ち主が人のproposal）のtaskとgoalの優先度と所属は自分で変えず（`lower_priority`を付けない）、疑いがあれば`concern`にする。人の言葉に優先度の無い新しいgoalにplannerが付けた段は下のAI由来のgoalと同じ目安で見てよいが、外れていれば`revise`にする。
- AI由来のproposalでは次を見る。taskには個別の優先度を置かずgoalから継がせる（`lower_priority`は値を置かず個別の指定を外す形で効き、passでruntimeも外す）。継いだ優先度が高すぎると見えるときは、下の所属かgoalの優先度の問題として扱う。新しいgoalの優先度が上の「goal の優先度とラベル」の段の目安から外れていれば`revise`にする（goalの優先度は自分で変えない）。既存のgoalに入れたtask（元goalのあるfollow_upは下の行による）は、そのgoalの受け入れ条件の達成に関係するかを見て、関係なければ`revise`にし、新しいgoalかgoalの無い単独のtaskにさせる（後回しの受け皿は上と同じ）。
- 依存は上の「依存の付け方」のとおり中身の前提に限る。優先度が下がる向き（高いgoalのtask → 低いgoalのtask）の依存は理由を見て、同じファイルの衝突を避けるためだけなら`revise`にし、種類と理由がnoteにもcontextにも無いときも`revise`にする。同じファイルやhotspotを触ることだけを理由に`add_dependency`を足さない（[ADR-t1985-1](../adr/2026-10-07-t1985-1-dependencies-only-for-content-prerequisites-and-conflicts-left-to-claim-deferral.md)決定4・6）。
- 元goalのあるfollow_upのdraftは、plannerの所属の判断（分類・acceptanceの項目・理由・証拠・所属先・判定時の版）を起点に検査する。手順と判定の基準はpluginの`dagq`の`reference/register.md`の「A follow_up's membership」、決定は[ADR-t1504-1](../adr/2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)と[ADR-t1504-2](../adr/2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)が持つ。疑わしいときにこのrepositoryで読む周辺の証拠は、元のrunのreceipt（`dagq events --full --run R --kind integration_receipt`）、元のtaskの差分（commit）、元goalのacceptanceの他の項目とdoc、名指されたADRと`docs/design/`の節。直せる対応づけの誤りは`revise`、follow_upを外すためにacceptanceを弱めたものは`concern`にする。
