---
id: development-task-registration
type: development
title: このrepositoryのtaskの登録（verify・paths・evidence・change・titleの選び方、固定バイナリを待つ宣言、依存の付け方、taskの大きさと分け方、Spikeのtask、負荷の下で落ちるtestを直すtask、ADRを書くtask、reviewのプログラムが読む形を変えるtask、ADRがリンクする文書を消すtask、runtimeの汎用性、文書の変更を求めるtask、plan reviewが当てはめる規則）
status: current
created: 2026-10-03
owners:
  - hisamekms
tags:
  - planning
  - conventions
related:
  - adr-t1985-1
  - adr-t1975-1
  - adr-t1971-1
  - adr-t1639-1
  - adr-t1639-2
  - adr-t1453-2
  - adr-t1942-1
  - adr-t1964-1
  - adr-t1504-1
  - adr-t1504-2
  - adr-t1591-1
  - adr-t1480-1
  - adr-t2257-1
  - adr-t2182-1
  - development-local-checks
  - development-testing
  - development-migrations
  - plan-local-checks-history
  - development-documents
  - plan-review-rounds-resume-spike
---

# このrepositoryのtaskの登録

このrepositoryでtaskを`dagq add`するときの`--verify`・`--paths`・`--evidence`・`--change`と、goal の優先度・ラベルの今の選び方。読むのは、taskを登録・修正するplannerと、proposalを見るplan review job（AGENTS.mdの「plan review」から辿る）。登録の汎用の手順（flagの意味、宣言外のpathを変えたrunの扱い、pathsの変え方）はpluginの`dagq`の`reference/scope.md`と`reference/register.md`、workerが手元で流すものは[手元の検証](local-checks.md)、testの規則は[testの制約](testing.md)が持つ。

## 推奨の組み合わせ

変更の対象で検証を選ぶ。globはrepository root起点で、`*`は1階層、`**`は任意の深さ。

- docsだけ: `--paths 'docs/**' --paths '*.md' --verify 'cargo fmt --all --check' --verify 'sh scripts/check-frontmatter-dates.sh' --verify 'sh scripts/check-doc-links.sh' --verify 'sh scripts/check-doc-frontmatter.sh' --verify 'sh scripts/check-design-docs.sh' --verify 'sh scripts/check-design-sentences.sh'`（fmtも要らなければ5本のscriptだけ。`check-frontmatter-dates.sh`はfrontmatterの日付の行（[frontmatter仕様](../frontmatter.md)が持たせるもの）にコメントがあれば、`check-doc-links.sh`は`docs/`とrootの`.md`の相対リンクが実在しないpathを指せば、`check-doc-frontmatter.sh`はADR以外の`docs/`の文書のfrontmatterが必須のkey・typeとstatusの値・idの形と重複で[frontmatter仕様](../frontmatter.md)に合わなければ落ちる検査で、どれも数秒で終わりCIも実行する。`check-design-docs.sh`は`docs/design/`の文書ごとの大きさ・1行の長さ・本文のtask番号の上限と許可の一覧を検査し、CIも実行する。上限と許可の一覧の扱いは[文書の規則](documents.md)の「design」。`check-design-sentences.sh`は`docs/design/`の差分が足した・変えた行の1文1行を検査し、CIとrunのreviewのプログラムも実行する。境界は同じ節の「検査の境界」）
- ADRを書く（docsだけ）: 上に`--verify 'sh scripts/check-adr-numbers.sh'`を足す（上の`check-design-docs.sh`と`check-design-sentences.sh`も残す）
- pluginの文書・skill: `--paths 'plugins/**' --paths 'docs/**' --paths '*.md' --verify 'cargo test --locked --test plugin' --verify 'sh scripts/check-frontmatter-dates.sh' --verify 'sh scripts/check-doc-links.sh' --verify 'sh scripts/check-doc-frontmatter.sh' --verify 'sh scripts/check-design-docs.sh' --verify 'sh scripts/check-design-sentences.sh'`（`check-design-docs.sh`と`check-design-sentences.sh`は上のdocsだけの行と同じ。`tests/plugin.rs`がskillの大きさと参照を検査するのでtestを残す。pluginの文書を読むtestはこれだけ）
- runtime（`src/`・`tests/`・`migrations/`）: `--paths`なしで`cargo fmt --all --check`・`cargo clippy --locked --all-targets -- -D warnings`・`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`・`sh scripts/check-design-docs.sh`・`sh scripts/check-design-sentences.sh`（runtimeのtaskは`docs/design/`も変えうるため。中身は上のdocsだけの行と同じ）。llvm-covの行は着地では`dagq.toml`の`[landing_verification]`がunit test全件と絞ったITに置き換え、coverageはCIが見る（下の「coverageの関門」）。`--evidence e2e`は一律には付けない（付ける目安は下の「e2e」）
- migrationを足す（runtime）: 上のruntimeの組み合わせに`--verify 'sh scripts/check-migration-numbers.sh'`を足す
- e2eの印（`.config/e2e-quarantine.toml`）を変える: verifyに`sh scripts/check-e2e-quarantine.sh`を付ける（書式・重複・testの実在・上限を検査し、期限切れは警告だけ。CIと、差分がそのpathに当たるrunの[プログラムのreview](../design/supervisor-lifecycle/review-programs.md)（`dagq.toml`の`[review.programs]`に挙げたとき）も実行する。印の規則は[testの制約](testing.md)の「e2eの印」）
- pluginとバイナリのversionを変える: verifyに`sh scripts/check-plugin-version.sh`を付ける（検査の中身は[plugin integration](../design/plugin-integration.md)の「tagとversionの一致規則」）
- itのtestの時間の関門の許可の一覧（`.config/it-slow-allow.toml`か`.config/it-slow-allow.d/*.toml`）か関門のscript（`scripts/check-it-test-time.sh`）を変える: verifyに`sh scripts/check-it-test-time.sh --self-test`を付ける（fixtureでscriptの照合を確かめ、このrepositoryの許可の一覧の書式を読む。関門そのものはCIが流す。許可の一覧に載せてよい理由は[testの制約](testing.md)の「判断と境界のtest」、書式は`scripts/check-it-test-time.sh`の冒頭のコメント）。新しい項目は足すtaskのファイル`.config/it-slow-allow.d/t<task ID>.toml`に足し、`.config/it-slow-allow.toml`には足さない（別々のtaskの追加が同じファイルに触れないため。そこの既存の項目を直す・外すのはよい）
- itのtestの境界の関門の宣言の一覧（`.config/it-boundary.d/*.toml`）か関門のscript（`scripts/check-it-added-tests.sh`）を変える: verifyに`sh scripts/check-it-added-tests.sh --self-test`を付け、scriptを変えるなら`sh scripts/check-scripts-root.sh`も付ける（fixtureでscriptの照合を確かめる。関門そのものはrunのプログラムのreviewとCIが流す。宣言してよい場合は[testの制約](testing.md)の「判断と境界のtest」、書式はscriptの冒頭のコメント）
- AGENTS.mdを変える: verifyに`sh scripts/check-agents-md-size.sh`を付ける（byteの上限を検査する。CIとrunのプログラムのreview（`dagq.toml`の`[review.programs]`に挙げたとき）も実行する。上限と、規則の本文をAGENTS.mdに足さないことは[文書の規則](documents.md)の「AGENTS.md」）
- 形式の検査のscript（`scripts/check-*.sh`）を足す・変える: verifyにそのscriptと`sh scripts/check-scripts-root.sh`を付ける。scriptは検査するtreeのrootをcwdのgitのtop（`git rev-parse --show-toplevel`）から取り、cwdがgitのwork treeの外のときだけscriptの場所から求める（runのプログラムのreviewはlanding branchのscriptを一時の場所に書き出し、cwdをrunのworktreeにして流すので、scriptの場所から求めるとreviewの対象のtreeを見ない）。`check-scripts-root.sh`の`scripts`の一覧にscriptを、`cases`にそのscriptの違反の場合と違反を置く`violate_<case>`を足す（HEADの一時のcloneにだけ違反を置き、写したscriptがexit 1・元のrepositoryではexit 0になることを確かめる。CIも実行する）。プログラムのreviewに挙げるscriptは、この一覧に入ってこの約束を満たすものだけにする。scriptがcwdから相対のpathでrepositoryの別のscriptを呼ぶと、reviewではworktreeの（workerが変えられる）版が動く（runtimeは検出しない）。cwdから設定を読むツールも同じで、`cargo`はworktreeの`rust-toolchain.toml`でworkerの選んだtoolchainを、`.cargo/config.toml`の`[alias]`で差し替えた外部のsubcommand（`fmt`・`clippy`など）やwrapperを動かしうる（integrateのverifyも同じworktreeのtoolchainと設定でcargoを流すのと同じ前提）。cargoなどを呼ぶscriptを挙げるなら、scriptの中で`RUSTUP_TOOLCHAIN`をlanding branchの値に固定するか、cwdの設定を読まない書き方にする（runtimeは強制しない）。`RUSTUP_TOOLCHAIN`はcargoの設定とbuildの`build.rs`を止めないので、worktreeをbuildするscriptは挙げない
- configだけ（設定と運用）: 変えるものだけを`--paths`に挙げる（`--paths 'dagq.toml'`・`--paths 'scripts/**'`・`--paths '.github/**'`・`--paths '.config/**'`・`--paths '.dagq/**'`・`--paths 'rust-toolchain.toml'`から選ぶ。runtimeのpathは含めない。下の「plan reviewが当てはめる規則」）。verifyは変えるものの検査: `dagq.toml`ならruntimeのparserが読めることを確かめる`--verify 'cargo test --locked --lib infrastructure::run_env::tests::this_repositorys_dagq_toml_has_the_rules_the_tests_classify_by'`（runtimeの`parse_config`でこのrepositoryの`dagq.toml`を読むので、構文だけでなくruntimeが読めることを見て、hostのpythonの版やinstallに依らない。testはtoolchainとtelemetryの規則がtestの前提と合うことも見るので、`dagq.toml`の変更で落ちたらそれは直すべき食い違い。hostのpythonは絶対pathが無いことや素の`python3`に`tomllib`が無いことがあり着地に失敗するので名指さない。`dagq.toml`がverificationをcontainerにした後もhostのpathは名指さず、projectのimageが入れる道具を名前（`cargo`）で名指す。そのtaskの検証をhostに回すことはしない。[ADR-t2182-1](../adr/2026-10-14-t2182-1-macos-specific-tests-are-left-to-the-ci-macos-job-not-the-container-verification.md)決定3）、scriptと`.config/`の印・一覧はこの節のそれぞれの行の検査（`check-e2e-quarantine.sh`・`check-it-test-time.sh --self-test`など）。検査の無いもの（CIのworkflowなど）は検証なし。新しいkeyを足すのは本番の固定バイナリが読めるようになってから（[運用](operations.md)の「`dagq.toml`を変えるとき」）
- `docs/`以下の`.md`を変えるtask（`--paths`に`docs/**`を含むtaskと、`--paths`なしのruntimeのtaskで`docs/design/`などを変えるもの）は、runtime・plugin・configのtaskでも、対象の行のverifyに加えて`--verify 'sh scripts/check-frontmatter-dates.sh'`・`--verify 'sh scripts/check-doc-links.sh'`・`--verify 'sh scripts/check-doc-frontmatter.sh'`を付ける。rootの`.md`（AGENTS.mdなど）だけを変えるtaskは`--verify 'sh scripts/check-doc-links.sh'`を付ける（着地の検証はtaskのverify（coverageの関門の置き換えを除く）だけなので、docsの行を取らないtaskでも日付の行のコメント・リンク切れ・frontmatterの欠けをpushの前に止めるため）。
- 対象が混ざるtaskは重い方の検証にする（`docs/`を変えるなら上の行の3本のscriptも足す）。

llvm-covとcargo testの重ね方:

- llvm-covをverificationに含めるtaskでは`cargo test --locked`をverificationに重ねない。着地ではllvm-covの行がunit test全件と絞ったITに置き換わり（[testの制約](testing.md)の「test binary」）、`cargo test --locked`を並べると置き換えの外でITを全部流し直すので、着地の検証を軽くした分が戻る。
- llvm-covを含めないtask（docs・pluginの文書など）は、必要なら`cargo test --locked`をverificationに残す。

## 固定バイナリがtask Xを含んでから行うtask

固定バイナリがtask Xの着地を含むことを要するtask（新しいflagの登録案内や、新しいkeyを使うconfigなど）は、`dagq add ... --wait-for-build --depends-on X`で登録する。必要な依存先が複数なら`--depends-on`を繰り返す。宣言は`wait_for_build`で、supervisorは自分のbuild識別子のcommitが直接の依存先の着地commitを全て含むまでclaimしない（[claimを控える](../design/supervisor-lifecycle/claim-hold.md#依存先の着地を含むbuildを待つtask)、[Domain model](../design/domain-model.md)）。draft / submittedのtaskは`edit TASK --wait-for-build`で宣言し、`--no-wait-for-build`で外せる。依存先が既に着地していても、buildを待つために依存を外さずに宣言する（`dagq lint`は`wait_for_build`のtaskの着地済みの依存先を指摘しない）。

verifyの1本目に、`~/.local/bin/dagq --version`のbuild識別子と依存先の着地を`git merge-base --is-ancestor`で比べる手書きのscriptを置かない。verifyはclaimの後にしか効かないため、早すぎるclaimを止めるのはこの宣言と`--depends-on`にする。自動更新はruntimeを変える着地でだけbuildするので、runtimeを変えない依存先だけを待つtaskには宣言しない（[ADR-t1632-1](../adr/2026-10-05-t1632-1-claim-waits-for-a-build-that-contains-the-dependencies-landings.md)）。

## pathsと軽い検証

変更の対象でverificationを軽くしてよい。条件は`--paths`で変えてよいパスを宣言すること（[ADR-0029](../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）。宣言外のパスを変えたrunはvalidatingで`needs_session`（`scope_violation`）になり、`integrate`もrebase後の差分を同じく検査して着地させないので、軽い検証のまま`src/`の変更が入ることはない。`--paths`を付けないtaskは制限されない。宣言外のパスが本当に要るときのworkerとplannerの手順はpluginの`dagq`の`reference/scope.md`の「What happens outside the paths」「Change the paths」が持つ。

## runtimeのtaskの主なファイル

runtimeのtaskは`--paths`を宣言しない（上の「推奨の組み合わせ」）ので、主に触るファイルをdescriptionに書く（例: 「主に`src/application/supervise/planning/plan_review.rs`と`tests/it/plan_review.rs`を触る」）。読み手（plan reviewの衝突の検出と`related`）、予想で制限でないこと、`--paths`に書かない理由はpluginの`dagq`の`reference/scope.md`の「Name the files a task without paths mainly touches」が持つ。

## coverageの関門

- 行カバレッジの80%の関門はCIだけが見る（[ADR-t1925-1](../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)決定1・5）。着地では、`dagq.toml`の`[landing_verification]`がtaskのverifyのcoverageの関門の行を、unit test全件と影響範囲で絞ったIT（既に落ちているtestを除く）の1本のコマンドに置き換え、他の行は登録のまま流す（決定4。仕組みは[Validation](../design/supervisor-lifecycle/validation.md#着地の検証)）。coverageを下回る変更も着地し、CIの見張りが修正taskにする。
- 新しく登録するruntimeのtaskも、verifyには上の「推奨の組み合わせ」のとおりcoverageの関門（`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`）を書く。この行が着地の検証に置き換わる印で、無いtaskは着地でunit testもITも流れない。`sh scripts/landing-it.sh`をverifyに直接書かない（`[landing_verification]`を外したときに登録のコマンドに戻せなくなる）。
- 登録済みのtaskの`cargo llvm-cov nextest --locked --fail-under-lines 80`と`cargo llvm-cov --locked --fail-under-lines 80`は書き換えない。dagqは単一のpackageなので`--workspace`の有無で測る範囲は変わらず（[ADR-t2126-1](../adr/2026-10-09-t2126-1-single-package-without-workspace-and-coverage-gate-keeps-workspace-flag.md)）、どちらの形も着地では同じく置き換わる（ADR-t1925-1決定4）。
- 着地の検証もCIもcargo-nextestで流すので、cargo-nextestが無いhost（[運用](operations.md)の「hostのツール」）では着地の検証が失敗する。旧コマンドで登録しても同じく置き換わるので避けられず、人がhostに入れる。

## e2e

- runtimeのtaskに`--evidence e2e`を一律には付けない。e2eの要否はvalidatingがrunの差分と`dagq.toml`の`[e2e] paths`から決めて`validation_finished`の`e2e_requirement`に記録し、要るrunにはreviewのpassの後にruntimeがhostでe2eを流す（[ADR-t963-1](../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定2・3、[ADR-t1233-2](../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)。仕組みは[Validation](../design/supervisor-lifecycle/validation.md)の「runtimeが流すe2e」と[着地の前のe2e](../design/supervisor-lifecycle/integrate.md#着地の前のe2e)。receiptの`e2e`のevidenceを求めないことも同じ）。
- `--evidence e2e`を明示したtaskは差分に依らずe2eが要るので、`[e2e] paths`の外でも実プロセス・launchdで確かめる必要があるとplannerが判断したときだけ付け、理由をdescriptionに書く。
- その判断の目安: runをまたぐsupervisorの振る舞い（superviseの回ごとのclaim・landing recheck・resume・`integrate --next`の着地の順・superviseの回ごとのrunの本数）を変え、`tests/e2e.rs`の複数passの筋書きを変えうるtaskには付ける。これらを変える差分は主に`[e2e] paths`の外のファイル（superviseの判断の部分（`src/application/supervise/`の`handoff.rs`・`handoff_slots.rs`と`update.rs`以外）や、claimと着地の順を決めるstoreとdomain）に収まるので、付けないと着地の前のe2eが流れない。task 1310（landing recheck）はこの外で着地の前のe2eが流れずに着地し、自動更新の関門で`tests/e2e.rs`の`two_independent_tasks_run_concurrently_and_a_dependent_follows_integration`が落ちて固定バイナリの更新が止まった（ask 350。testはtask 1516が直した）。`[e2e] paths`にsuperviseの判断のファイルを足して防がないのは、runtimeのcommitの多くが触るので狭める効果がほぼ消えるため（`dagq.toml`のコメントとtask 1516の判断）。目安に当たらないsuperviseの変更（1つのrunの中の判断だけを変えるもの）には付けない。

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
- `dagq.toml`の`[supervisor] light_changes`に置いたchange（このrepositoryではdocsとconfig）のtaskは、着地の順番を待つだけのrunが空けた枠でもclaimされうるが、そこでclaimされるのは`--paths`を宣言したtaskだけなので、docs・configのtaskには`--paths`の宣言が要る（[ADR-t1591-1](../adr/2026-10-04-t1591-1-landing-queue-leaves-room-for-light-changes.md)決定2・3、判定は[claimを控える](../design/supervisor-lifecycle/claim-hold.md#着地待ちが空けた軽い枠)）。軽い枠に重い変更が紛れると、`parallel`と`[run.env]`が前提にする重いbuildの同時数を超えるため。宣言の外を変えたrunは軽い枠のものも上の「pathsと軽い検証」のとおり止まる。
- 混ざるときは主な目的の1つを選ぶ（例: 不具合の修正にtestを足すなら`fix`、新しい機能のdocsを同じtaskで書くなら`feature`）。changeは検証を決めない（検証は上の推奨の組み合わせのとおり、変更の対象で選ぶ）。
- 作業時間の前後比較で読む層は[運用](operations.md)の「KPIの読み方と印」。

## taskのtitle

- taskのtitleは[文書の規則](documents.md)の「commit」の7つの接頭辞のどれかで始める（`docs: ...`の形）。titleはmainのsquash commitの件名になり、制御側のgitはhookを走らせないので、mainの接頭辞はtitleで揃える（[ADR-t2596-1](../adr/2026-10-17-t2596-1-seven-commit-prefixes-checked-by-hook-review-program-and-task-title.md)決定4）。登録するplannerが付け、plan reviewが見る（下の「plan reviewが当てはめる規則」）。
- `dagq lint`と`submit`は`dagq.toml`の`[tasks] title_prefixes`でtitleの接頭辞を検査する（lintは報告し、submitは拒む）。
- 接頭辞は上の「change」から選ぶ: `feature`→`feat`、`fix`→`fix`、`refactor`→`refactor`（速さの改善が目的なら`perf`）、`test`→`test`、`docs`→`docs`、`measure`→`chore`（結果の文書だけを変えるなら`docs`）、`config`→`chore`。

## goal の優先度とラベル

この repository のラベルの語彙は `dagq.toml` の `[goals] tags` が正本で、[棚卸しの案](../plans/goal-priority-inventory.md#1-ラベルの語彙の案)（task 1642）の 11 語を採用する。ラベルはテーマを表し、優先度は表さない。goal の主題に合うものを 1 つ以上付け、主なラベルを先頭に置く。複数のテーマに関わるときは該当するものを添える。変更するファイルの種類は `[areas]`、task の変更の種類は `change` が持つので、それだけを理由にラベルを選ばない。

| ラベル | 意味 | 付ける基準 |
|---|---|---|
| `headless` | worker・planner・job を非対話（turn ごとの呼び出し）と background の wrapper で動かすこと | 非対話の経路・wrapper・その評価と測定を作るか変える goal。非対話の run の stall や待ちの改善が主題なら `reliability` を主にして `headless` を添える |
| `codex` | Codex の provider（worker・job・planner・inbox）を使えるようにし、Claude と比べること | provider を Codex に広げるか切り替える goal。レビュー系かどうかでラベルを分けず、優先度は goal ごとに判断する |
| `cmux` | dagq から cmux への依存を無くした後の残り（古い記録の読み方・受け付けて無視する引数・案内）を整理すること | cmux への依存を無くした後に残る記録・引数・案内を整理する goal |
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

この目安は AI が付ける goal（AI 由来の新しい goal と、人の言葉に優先度の無い新しい goal）に当てはめる。人が明示した優先度（`interrupt` を含む）は目安で変えない。人の言葉にある優先度は inbox と人が `dagq request add` の `--priority` に入れ、runtime がその request の planner が作った goal（と、その request の由来の task のうち、値が違うか出どころが `human` でない goal に足したものと goal の無いもの）に人の出どころ（`human`）の優先度として付けるので、planner は request の文から読んで goal に書き写さない（[ADR-t1975-1](../adr/2026-10-08-t1975-1-runtime-records-origin-and-person-set-priority-and-ai-actors-cannot-change-it.md) 決定1・2・5）。人の出どころの優先度は段の目安で変えず、変えるのは人と inbox だけで、AI の actor の `set-priority`・`goal edit` は runtime が拒み、AI の actor の所属の変更では runtime が値を task に残す（同 決定3・4）。ただし follow_up を `out_of_scope` の判断で移すときは、元の goal から継いでいただけの値は残さず移動先の goal を継ぎ、task に直接付いた人の値だけが残る（[ADR-t2373-1](../adr/2026-10-12-t2373-1-out-of-scope-follow-up-move-takes-the-destination-goals-priority.md)）。人の言葉に優先度の無い新しい goal に目安で段を付けたときは、そのことを goal の description か task の context に書く。AI 由来の task（由来は runtime が goal・task の作成時に記録する。[ADR-t1975-1](../adr/2026-10-08-t1975-1-runtime-records-origin-and-person-set-priority-and-ai-actors-cannot-change-it.md) 決定5・6）には個別の優先度を付けず goal から継がせ、優先度を変えたいときは所属か goal の優先度を見直す。

2026-10-02 の段と 2026-10-03 の方針は棚卸しの初期値の案の材料で、テーマと段を恒久的に結び付ける規則ではない。既存の goal ごとの優先度案と今の task の値とのずれは[棚卸しの「2. goal ごとの表」](../plans/goal-priority-inventory.md#2-goal-ごとの表)にあり、値の適用と今の並びとの調整は人が別に決める。この設定と文書の変更では既存の goal・task の値や所属を変えない。

受け皿は元の goal ごとの積み残しではなく、同じラベルのテーマごとにまとめ、優先度を `low` にする（[ADR-t1639-2](../adr/2026-10-04-t1639-2-defer-improvements-outside-acceptance-to-a-low-goal-per-tag.md)）。goal・task の優先度の付け方と継承、後回しの判定・所属の記録・受け皿の選択と移動の汎用の手順は plugin の `dagq` の `reference/register.md` が持つ。

## 依存の付け方

- taskの依存（`--depends-on`）は中身の前提（先に着地しないと作業が成り立たない）に限る。同じファイルの衝突を避けるためだけの依存は付けず、askにもしない。同じファイルの衝突は、衝突の多いファイルならruntimeのclaimの控え（[claimを控える](../design/supervisor-lifecycle/claim-hold.md#衝突の多いファイル)。上限の時間まで、`interrupt`のtaskは控えない）が扱い、残りはrebaseか着地の衝突として解く（[ADR-t1985-1](../adr/2026-10-07-t1985-1-dependencies-only-for-content-prerequisites-and-conflicts-left-to-claim-deferral.md)決定1）。
- 依存を付けるときは、noteかtaskのcontextに種類（中身の前提か衝突回避か）と理由を書く（決定2）。
- 人に聞く（`planner_question`、`--because scope`）のは、(a) 中身の前提の依存で、(b) その依存で低い優先度のgoalのtaskが効く優先度を継いで上がり、(c) 順番の変更・taskのgoalへの取り込み・分割などplannerの手で解けない、の全部を満たすときだけにする。問いは「低いgoalのtaskを（優先度ごと）前に出すか、高いgoalが待つか」にする。どれかを満たさなければ自分で決めて進め、理由をnoteかcontextに残す（決定3）。

## taskの大きさと分け方

reviewが3回以上になるrunは差分の大きいtaskに偏り、そのうち詳細に調べた30 runでは、reviewの取りこぼし（前の回に出せた指摘が後の回に出る）が差分の特に大きいrunに集まった（[review の回と resume のスパイク](../plans/review-rounds-resume-spike.md)の「表 C（H3）」と「run ごとの表」）。そこで、登録するplannerはtaskごとに差分の大きさを予想し、目安を超えるtaskは分けるか、分けない理由を書く。

- 目安: 予想する差分（testと文書を含む追加＋削除の行数）が約650行か、変えるfileが約12を超えるtask。reviewが3回以上のrunの差分の中央値が653行・12 fileで、2回以下のrunの129.5行・2 fileの約5倍・6倍だった。この値は最後の回の差分で回の間の直しを含むが、最初の回の差分でも464行・9 fileで、2回以下のrunの3.6倍・4.5倍だった。
- 大きく超える目安: 予想が2,500行を超えるtask。詳細調査の30 runの取りこぼし22件のうち17件が、差分が2,500行を超える11 runのうちの7 runに集まった。
- 予想は、runtimeのtaskはdescriptionに書く主に触るfile（上の「runtimeのtaskの主なファイル」）、docs・configのtaskは`--paths`とdescriptionが名指す文書・設定から、足す・変える振る舞いの数と合わせて立てる。
- 受け入れ条件の項目数は目安にしない。同じ表 Cで、項目数の中央値の比（3回以上 ÷ 2回以下）は0.29（「1. 2.」や「(a)」の番号も数えると1.25）で、偏りは差分の行数とfile数にだけ出た。
- 分け方: 一つ一つが単独で着地でき、着地した時点でmainのtestと検証が通る単位に分ける（例: 型と保存の層を先に、それを使う判断と表示を後に。仕組みを先に、設定と文書の切り替えを後に）。後の単位が先の単位の中身を前提にするときだけ、上の「依存の付け方」のとおり中身の前提の依存を付け、種類と理由をnoteかcontextに書く。受け入れ条件も単位ごとに分け、各taskが自分の条件だけで着地できるようにする。
- 分けないとき: 一つのtaskにし、予想する大きさと分けない理由をtaskのcontextに書く。目安を超え2,500行以下の予想なら、理由は分けても単位ごとのreviewが軽くならない（変更が一つの流れに収まり、単位に分けると説明と検証が重なる）などでよい。2,500行を超える予想では、途中の状態ではmainの検証が通らない、同じ不変条件を一度に変えないと壊れる、のように単独で着地できる単位に分けられない理由に限る。
- plan reviewの扱いは下の「plan reviewが当てはめる規則」の大きさの行が持つ。

## Spikeのtask

前提の抽出・短い読み取りの範囲・Spikeの形と登録・submitの後に終わること・再計画の汎用の手順はpluginの`dagq`の`reference/spike.md`が持ち、決定は[ADR-t1487-1](../adr/2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)が持つ。ここはこのrepositoryで何を重要な前提とし、何で解き、どこに証拠を置き、どう登録するかだけを持つ。

- 重要な前提の目安: このrepositoryで計画を左右しやすいのは、cmuxの振る舞い、Git worktree（作り方・掃除・同時の操作）、provider（Claude・Codex）とheadlessの経路（非対話の呼び出し・sandbox・resume）、authorizationの権限（actorとcapability）、hostのツール（sccache・cargo-nextestなど、[運用](operations.md)の「hostのツール」）、loadと並列度（`parallel`と`[run.env]`のbuild・testの並列度）である。
- 短い読み取りの根拠にしてよいもの: コード（`src/`のdoc commentとtest）、ADR（索引の`docs/adr/INDEX.md`）、`docs/design/`、queueの`events`・receipt・`stats`・`kpi`。
- 証拠の所在: 結果は`docs/plans/<名前>-spike.md`（`type: plan`）に書き、環境（対象のcommit・道具の版・providerと経路・host）、打ったもの、問いごとの判定と根拠、後続の計画への推奨の節を持たせる。acceptanceには判定と根拠を書くその文書の節と、同じ文書に対象のcommit・道具の版・環境を書くことを名指す。文書の形の前例は[spike-predictor-replay](../plans/spike-predictor-replay.md)（task 553、過去のrunの再現で測る）、[headless-worker-spike](../plans/headless-worker-spike.md)（task 812）、[codex-headless-jobs-spike](../plans/codex-headless-jobs-spike.md)（task 1061、実CLIと本番queueで確かめる）。task 812は使い捨てのqueueを使ったので、方法の前例にはしない（下の箇条）。
- 権限・headless・providerの前提は、本番queueのSpikeのrunそのもの（実際のworkerの経路）で確かめる。使い捨てのqueueを使う手作業をacceptanceにしない理由は、AGENTS.mdの「本番 queue と開発環境の境界」と[運用](operations.md)の「workerがhostと実queueでできないこと」。
- verify・paths: 読んで測って文書に書くだけのSpikeは上の「推奨の組み合わせ」のdocsだけの行、計装や試作で`src/`・`tests/`に触るSpikeはruntimeの行に、結果を書く`docs/plans/`のための同じ節の`docs/`を変えるtaskの3本のscriptを足す。
- change: 実行区分と独立に、上の「change」のとおり主な目的の1つを選び、Spikeだからと`measure`に固定しない（数える・測るのが主なら`measure`、試作で機能の前提を確かめるなら`feature`など）。実験やbuildで作業が長いSpikeは、結果の文書だけを変えても`docs`にしない（`light_changes`の軽い枠でclaimされうる）。
- 区分の上限（`dagq.toml`の`max_spikes`・`max_investigations`）を変えるときの見直しの条件は[運用](operations.md)の「`dagq.toml`を変えるとき」。

## 負荷の下で落ちるtestを直すtask

- 負荷の下で落ちるtestを直すtaskのacceptanceとdescriptionに、高い負荷の下での再現を求めない（[ADR-t1480-1](../adr/2026-10-05-t1480-1-workers-add-no-load-to-the-host-to-reproduce-failures-under-load.md)決定(d)）。求めないもの: loadの値の指定（「load average 20以上で」など）、`-j` / `--test-threads`の`[run.env]`の並列度を超える引き上げ（`-j 16`など）、同時の複数のstressかtestのprocess、負荷をかける処理（`yes`・busy loop・stressの道具、「他のrunか負荷をかける処理」）。workerはそれらをhostで起動しないため（[手元の検証](local-checks.md)の「負荷の下で落ちるtestの再現」）。
- 代わりに、再現と確かめ方は記録（eventのdump・log）を読む、待っている条件と上限を確かめる、testの中で遅れを決定的に作る（stubの遅延、上限を縮めるなど）、1本のprocessでの上限つきの繰り返し（上限の数値は同じ節）から選んで書き、それで再現しない稀な失敗は直せる範囲を直して失敗のときの出力を増やし、follow_up（`flaky_test`）とCIの定時実行（[ADR-t920-1](../adr/2026-09-28-t920-1-light-worker-stress-and-heavy-repetition-in-scheduled-ci.md)）に任せる、と書く。
- 例外は、plan reviewのapprove_planのaskに人がreadyと答えて、高い負荷の下での確認をacceptanceに持つtaskを認めたものだけ（ask 323のtask 1360・1361（event 62021・62046に承認と適用）、ask 304のtask 1344）。これらは上の対象外で、workerはacceptanceどおりに確かめてよい（acceptanceの範囲を超えて負荷は足さない）。新しい例外は人のapprove_planの答えでだけ認められ、plannerもplan reviewも自分では例外を作らない（決定(e)）。既存の例外を取り消すときも、人の判断を得てから1360・1361を計画し直す。

## ADRを書くtask

- ADRを書くtaskを登録するときは、descriptionに本数と各IDの中身を書く（自分のIDは`add`が返すまで分からないので「このtaskのIDでADR-t<ID>-1を書く」と書くか、`add`の後にdraftを直す）。verifyは上の「推奨の組み合わせ」の「ADRを書く」の行。IDの形と、番号の割り当ての棚卸しをしないことは[文書の規則](documents.md)の「ADRのID」。
- 番号付きの決定を複数持つADRの一部を変える（`amends`）か、丸ごと置き換えるかを、ADRを書くtaskのplannerがdescriptionに書き、plan reviewが見る（下の「plan reviewが当てはめる規則」）。選び方は[文書の規則](documents.md)の「ADR」。
- 現行のADRの決定（方針・境界・不変条件）に触れるtask（変更がADRの決定の範囲に入るか、descriptionかacceptanceがADRを名指すtask）は、ADRを書くかどうかを登録の時に次のどちらかに確定してdescriptionに書く。
  - (a) ADRを書かない: 保つ決定を「ADR-X決定Nを保つ」の形で名指し、細部はdesignかdoc commentに書くと書く。
  - (b) ADRを書く: 上の2項のとおり本数・ID・変える決定・`amends`か置き換えかを書き、verifyに`sh scripts/check-adr-numbers.sh`を足す。
  - 名指すADRは、frontmatterの`superseded_by`・`amended_by`を辿った今の決定にする（辿り方は[frontmatter仕様](../frontmatter.md#adr-fields)の「ADR fields」）。`superseded`のADRを今の決定として引かない。
  - 「必要ならADRを書く」のようにworkerに選ばせる書き方と、既存のADRの本文（Consequencesを含む）を直す選択肢は書かない（ADRがappend-onlyであることは[文書の規則](documents.md)の「ADR」）。決定を変える必要が実装中に分かったら、workerはaskで止める、と添える。
- 決定と実装が明らかなものは、ADRと実装を1 taskにする（[ADR-t598-1](../adr/2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)決定12）。

## reviewのプログラムが読む形を変えるtask

- runのプログラムのreviewは、runのcommitでなくlanding branch（main）の今のcommitから`[review.programs.<name>]`とそのscriptを読んで流す（[プログラムのreview](../design/supervisor-lifecycle/review-programs.md)の「読む元・流す場所・env・結果」のsnapshot）。
  このため、programのscriptが読む設定・一覧（`.config/`など）や、scriptが前提にするコードの形を消す・変えるtaskは、同じrunでscriptを直してもreviewではmainの旧いscriptが動き、新しいtreeが落ちる。
- そのようなtaskを登録するときは、旧い形と新しい形の両方を通すprogramの変更（またはprogramの退役）を先のtaskにし、形を変えるtaskをそれに`--depends-on`で依存させる（中身の前提の依存。先の変更が既にmainにあれば要らない）。
- 先のtaskが両方の形を通せないときは、旧い形を残したまま新しい形を足す互換のtask、新しい形を読むprogramの更新（または退役）のtask、旧い形を消すtaskの3つに分ける。
  旧い形を消すtaskはprogramの更新（または退役）のtaskに`--depends-on`で依存させ、互換のtaskへの依存だけで代えない（互換のtaskの着地はmainのscriptを変えないため）。

## ADRがリンクする文書を消すtask

- append-onlyのADRがリンクする文書を消すtaskは、上の「reviewのプログラムが読む形を変えるtask」の一例として、`scripts/check-doc-links.sh`の削除した文書の一覧にその文書のpathと移管先を移行中の行として足すconfigのtaskを先に登録し、消すtaskをそれに`--depends-on`で依存させる（行の意味は[文書の規則](documents.md)の「ADR」と「frontmatter」）。
  移行中の行はpathに文書が在っても通るので、両方の形を通す先のtaskとして先に着地させてよい。

## runtimeの汎用性

dagqは外部のprojectで使い、このrepositoryはdogfoodingしているだけなので、runtimeはこのrepositoryの開発の事情を決まりや既定値にしない（[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定1）。

- runtimeのtaskが、言語・道具（Rust・cargo・cargo-nextest・sccacheなど）・外への宛先（package registryなど）・hostで流すコマンド（e2e・OSに固有のtest）・compileのcache・資源の値をruntimeに固定していれば、設定に出す形にする。
  runtimeは仕組みと、どのprojectにも通じる既定（無ければ使わない・拒む側）を持ち、projectに固有の値は`dagq.toml`、hostに固有の値は`host.toml`で受ける。
- このrepositoryの値は、その設定のdogfoodingの値として登録する: `dagq.toml`に置く値はconfigのtask（新しいkeyは固定バイナリが読めるようになってから。上の「固定バイナリがtask Xを含んでから行うtask」）、`host.toml`の推奨値はdocsのtaskにする。
- pluginの汎用性は[文書の規則](documents.md)の「pluginの汎用性」が持つ。

## 文書の変更を求めるtask

descriptionかacceptanceが文書の変更を求めるtaskは、登録の時に次に合わせて書く（規則の本文は[文書の規則](documents.md)が持つ）。

1. 既定値・閾値の数値、設定のkeyと型、eventの欄、CLIのflag、JSONの形、関数とファイルの名前の列挙は`docs/design/`に書かせず、意味が要るなら定義のそばのdoc commentに書かせる。
   designに求めるのは流れ・境界・不変条件・コードから読めない約束と、名前で指すコードへの入口にする（[文書の規則](documents.md)の「design」、[ADR-t1942-1](../adr/2026-10-07-t1942-1-design-docs-in-four-layers-with-size-budgets.md)）。
2. designにtask・goalの番号と「以前は」のような経緯を書かせない（同じ「design」）。
3. designを含むADR以外の文書に`updated`・`last_verified`の更新を求めない（[文書の規則](documents.md)の「frontmatter」、[ADR-t1964-1](../adr/2026-10-07-t1964-1-non-adr-docs-drop-updated-and-last-verified.md)）。
4. designの記述を消させるときは、[文書の規則](documents.md)の「形と予算」の1・2・4に合わせる。
   doc commentへ移させるのはコードから読める細かい事実で他に記述の無いものに限り、follow_upに回すだけにせず同じ変更で移させ、地図として要る約束・入口は残すか適切な文書へ移させる。
5. pluginのskill・referenceに書かせる文言に、このrepositoryのADRの番号と固有の規則を入れない（[文書の規則](documents.md)の「pluginの汎用性」、[ADR-t1453-2](../adr/2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)決定1）。

## plan reviewが当てはめる規則

この repository のplan review jobは、proposalのtaskに次を当てはめる（読む文書はAGENTS.mdの「plan review」が名指す）。

- 固定バイナリが依存先の着地を含むことを要するtaskは、上の「固定バイナリがtask Xを含んでから行うtask」のとおり`--wait-for-build`の宣言と必要な`--depends-on`を持ち、verifyの手書きの関門で代用していないこと。宣言や依存が欠けている、または手書きの関門で代用していれば`revise`にする。
- verify・paths・evidenceは上の「推奨の組み合わせ」（llvm-covと`cargo test`の重ね方を含む）と「e2e」に合い、changeは上の「change」のとおりtaskの主な目的に合う1つであること。runtimeのtaskに一律の`--evidence e2e`は求めないが、上の「e2e」の目安（runをまたぐsupervisorの振る舞いを変え、`tests/e2e.rs`の複数passの筋書きを変えうる）に当たるtaskに`--evidence e2e`が無ければ`revise`にする。`--evidence e2e`が付いていれば、`[e2e] paths`の外でも実プロセス・launchdで確かめる理由（目安に当たることを含む）がdescriptionにあるかを見る。
- changeが`docs`か`config`のtaskは`--paths`を宣言し、そのglobがruntimeのpath（`src/`・`tests/`・`migrations/`と、buildの設定の`Cargo.toml`・`Cargo.lock`・`build.rs`）に当たらないこと。`docs`は上の「推奨の組み合わせ」のdocs・pluginの文書の行のpaths、`config`は同じ節のconfigの行のpathsの範囲に収める。`--paths`が無い・runtimeのpathを含む（`**`のような広いglobを含む）・descriptionやacceptanceがruntimeのpathの変更を求める、といった食い違いは`revise`にし、主な目的に合うchange（`feature`・`fix`・`refactor`など）に直すか`--paths`を絞らせる。上の「change」の軽い枠で重いbuildが走らないようにするため（[ADR-t1591-1](../adr/2026-10-04-t1591-1-landing-queue-leaves-room-for-light-changes.md)決定3。軽い枠でclaimしてよいかの判定はruntimeが持ち、この規則は登録の時にpathsの中身を見る）。
- `docs/`以下の`.md`を変えるtask（`--paths`に`docs/**`を含むか、descriptionやacceptanceが`docs/`の文書の変更を挙げるもの。runtime・plugin・configのtaskを含む）のverifyに`sh scripts/check-frontmatter-dates.sh`・`sh scripts/check-doc-links.sh`・`sh scripts/check-doc-frontmatter.sh`のどれかが無ければ`revise`にする（上の「推奨の組み合わせ」の`docs/`を変えるtaskの行）。
- `docs/design/`を変えうるtask（`--paths`に`docs/**`か`docs/design/`に当たるglobを含むtask、`--paths`なしのruntimeのtask、descriptionやacceptanceが`docs/design/`の文書の変更を挙げるtask）のverifyに`sh scripts/check-design-docs.sh`か`sh scripts/check-design-sentences.sh`が無ければ`revise`にする（上の「推奨の組み合わせ」のdocsだけ・ADR・pluginの文書・runtimeの行）。configだけのtaskには求めない。
- 測定のtask（changeが`measure`のtaskと、受け入れ条件に測定を含むtask）は、周回数（と交互に流すか）、表の列、値の計算式（何を何で割るか、待ちを引くときの区間）、証拠の所在（文書の節・CSV・script・コマンドと時刻の区切り）をacceptanceかdescriptionに書くこと（測定の形に当たらない項目、例えば1回だけ読む測定の周回数は、当たらない理由を書く）。条件の範囲を「同じ形のもの」で広げるtaskは、範囲を決めるgrepか一覧を書くこと。欠けていれば`revise`（workerはこれらを根拠に受け入れ条件の各項目を対応づける。[ADR-t1420-1](../adr/2026-10-03-t1420-1-worker-maps-each-acceptance-criterion-before-the-receipt.md)）。
- ADRの索引（`docs/adr/INDEX.md`、無ければ`sh scripts/adr-index.sh`で作る。[文書の規則](documents.md)の「ADR」）と、taskが名指すADRと`docs/design/`の文書を読み、`accepted`のADRの決定と矛盾するtaskは`concern`にする（`superseded`なら`superseded_by`を辿る）。
- `dagq.toml`の`[review.programs.<name>]`のscriptが読むpath（設定・一覧）やscriptが前提にするコードの形を消す・変えるtaskが、上の「reviewのプログラムが読む形を変えるtask」のとおり、その形を通すprogramの変更（または退役）のtaskに`--depends-on`で依存していること（その変更が既にmainにあれば要らない）。
  旧い形を消すtaskが互換のtaskにだけ依存し、programの更新（または退役）に依存していなければ、それも欠けとみる。
  ADRがリンクする文書を消すtaskの移行中の行を足すtaskは、両方の形を通すprogramの変更に当たる。
  欠けていれば`revise`にする。
- taskのtitleが上の「taskのtitle」の接頭辞で始まり、changeに合うこと。外れていれば`revise`にする。
- ADRを書くtaskが上の「ADRを書くtask」を満たすこと（IDとファイル名の形、`check-adr-numbers.sh`のverify、置き換えか`amends`か）。足りなければ`revise`。
- 現行のADRの決定に触れるtaskが、上の「ADRを書くtask」のとおりADRの扱い（(a)保つ決定を名指して書かない、か(b)書く）をdescriptionに確定していること。workerに選ばせる・既存のADRの本文（Consequencesを含む）の変更を許す・`superseded`のADRを今の決定として引く、のどれかに当たれば`revise`にする。
- runtimeのtaskが上の「runtimeの汎用性」のとおり、言語・道具・外への宛先・hostで流すコマンド・cache・資源の値をruntimeに固定せず設定で受けること。固定していれば`revise`にし、設定に出す形と、このrepositoryの値を`dagq.toml`かdocsのdogfoodingの値として登録する形に直させる（[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)）。
- 文書の変更を求めるtaskのdescriptionとacceptanceが上の「文書の変更を求めるtask」の1〜5に反していれば、`concern`でなく`revise`にする（直し方が規則から一意に決まり人の判断が要らないため）。ADRの決定と矛盾して人の判断が要るものは上のADRの行のとおり`concern`にする。
- 挙動や仕様を変えるtaskは、関連文書（[文書の規則](documents.md)の「workerの文書の照合」が挙げる文書）のpath・節と更新が要る理由をdescriptionかcontextに書くこと。欠けていて関連する文書が明らかなら、見つけたpathを理由に書いて`revise`にする。文書の差分を求めるverifyやevidenceは求めない（[ADR-t1428-1](../adr/2026-10-03-t1428-1-decide-the-documents-to-update-when-the-code-changes.md)）。
- 負荷の下で落ちるtestのtaskが上の「負荷の下で落ちるtestを直すtask」を満たすこと。acceptanceかdescriptionが高い負荷の下での再現を求めるtaskは`revise`にする（[ADR-t1480-1](../adr/2026-10-05-t1480-1-workers-add-no-load-to-the-host-to-reproduce-failures-under-load.md)決定(d)）。ただし人がapprove_planのaskにreadyと答えて認めたtask（ask 323のtask 1360・1361、ask 304のtask 1344）は例外として保ち、`revise`にしない。plan reviewは自分で新しい例外を作らない（決定(e)）。
- 優先度と所属は由来で分けて見る（[ADR-t1971-1](../adr/2026-10-07-t1971-1-plan-review-keeps-human-origin-priority-and-membership-and-ai-tasks-inherit-goal-priority.md)）。由来はproposal単位でなく、runtimeがgoal・taskの作成時に記録した由来で見る（[ADR-t1975-1](../adr/2026-10-08-t1975-1-runtime-records-origin-and-person-set-priority-and-ai-actors-cannot-change-it.md)決定5〜7。由来が`unknown`の既存の行は人間由来と同じく扱う）。人間由来のtaskとgoalの優先度と所属は自分で変えず（`lower_priority`を付けない）、疑いがあれば`concern`にする。由来を問わず、人の出どころ（`human`）の優先度（taskの個別の指定と、taskが継ぐgoalの優先度）はactionで変えず（付けても適用されず記録だけが残る）、疑いがあれば`concern`にする（同決定3）。人の言葉に優先度の無い新しいgoalにplannerが付けた段は下のAI由来のgoalと同じ目安で見てよいが、外れていれば`revise`にする。
- AI由来のtask・goalでは次を見る。taskには個別の優先度を置かずgoalから継がせる（`lower_priority`は値を置かず個別の指定を外す形で効き、passでruntimeも外す）。継いだ優先度が高すぎると見えるときは、下の所属かgoalの優先度の問題として扱う。新しいgoalの優先度が上の「goal の優先度とラベル」の段の目安から外れていれば`revise`にする（goalの優先度は自分で変えない）。既存のgoalに入れたtask（元goalのあるfollow_upは下の行による）は、そのgoalの受け入れ条件の達成に関係するかを見て、関係なければ`revise`にし、新しいgoalかgoalの無い単独のtaskにさせる（後回しの受け皿は上と同じ）。
- taskの大きさは上の「taskの大きさと分け方」のとおり見る。目安を超えると予想できるtask（runtimeのtaskはdescriptionの主に触るfileが約12を超える、docs・configのtaskは`--paths`とdescriptionが名指す文書・設定が約12を超える、独立に着地できる変更を複数束ねている、など）で、contextに予想する大きさと分けない理由が無ければ`revise`にし、分けるか理由を書かせる。理由があれば、それが同じ節の「分けないとき」に当たるか（2,500行を超えると予想できるものは単独で着地できる単位に分けられない理由か）だけを見て、大きさだけを理由に`revise`にしない。受け入れ条件の項目数を大きさの根拠にしない。
- 依存は上の「依存の付け方」のとおり中身の前提に限る。優先度が下がる向き（高いgoalのtask → 低いgoalのtask）の依存は理由を見て、同じファイルの衝突を避けるためだけなら`revise`にし、種類と理由がnoteにもcontextにも無いときも`revise`にする。同じファイルやhotspotを触ることだけを理由に`add_dependency`を足さない（[ADR-t1985-1](../adr/2026-10-07-t1985-1-dependencies-only-for-content-prerequisites-and-conflicts-left-to-claim-deferral.md)決定4・6）。
- 元goalのあるfollow_upのdraftは、plannerの所属の判断（分類・acceptanceの項目・理由・証拠・所属先・判定時の版）を起点に検査する。手順と判定の基準はpluginの`dagq`の`reference/register.md`の「A follow_up's membership」、決定は[ADR-t1504-1](../adr/2026-10-04-t1504-1-follow-ups-belong-to-the-goal-whose-acceptance-needs-them.md)と[ADR-t1504-2](../adr/2026-10-04-t1504-2-runtime-records-and-enforces-follow-up-membership-judgements.md)が持つ。疑わしいときにこのrepositoryで読む周辺の証拠は、元のrunのreceipt（`dagq events --full --run R --kind integration_receipt`）、元のtaskの差分（commit）、元goalのacceptanceの他の項目とdoc、名指されたADRと`docs/design/`の節。直せる対応づけの誤りは`revise`、follow_upを外すためにacceptanceを弱めたものは`concern`にする。
