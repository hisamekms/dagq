---
id: development-task-registration
type: development
title: このrepositoryのtaskの登録（verify・paths・evidence・changeの選び方と、plan reviewが当てはめる規則）
status: current
created: 2026-10-03
updated: 2026-10-03
owners:
  - hisamekms
tags:
  - planning
  - conventions
related:
  - adr-t1453-2
  - development-local-checks
  - development-testing
  - development-migrations
  - plan-local-checks-history
---

# このrepositoryのtaskの登録

このrepositoryでtaskを`dagq add`するときの`--verify`・`--paths`・`--evidence`・`--change`の今の選び方。読むのは、taskを登録・修正するplannerと、proposalを見るplan review job（AGENTS.mdの「plan review」から辿る）。登録の汎用の手順（flagの意味、宣言外のpathを変えたrunの扱い、pathsの変え方）はpluginの`dagq`の`reference/scope.md`と`reference/register.md`、workerが手元で流すものは[手元の検証](local-checks.md)、testの規則は[testの制約](testing.md)が持つ。

## 推奨の組み合わせ

変更の対象で検証を選ぶ。globはrepository root起点で、`*`は1階層、`**`は任意の深さ。

- docsだけ: `--paths 'docs/**' --paths '*.md' --verify 'cargo fmt --all --check'`（fmtも要らなければ検証なし）
- ADRを書く（docsだけ）: 上に`--verify 'sh scripts/check-adr-numbers.sh'`を足す
- pluginの文書・skill: `--paths 'plugins/**' --paths 'docs/**' --paths '*.md' --verify 'cargo test --locked --test plugin'`（`tests/plugin.rs`がskillの大きさと参照を検査するのでtestを残す。pluginの文書を読むtestはこれだけ）
- runtime（`src/`・`tests/`・`migrations/`・`crates/`）: `--paths`なしで`cargo fmt --all --check`・`cargo clippy --locked --all-targets -- -D warnings`・`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`。`--evidence e2e`は付けない（下の「e2e」）
- migrationを足す（runtime）: 上のruntimeの組み合わせに`--verify 'sh scripts/check-migration-numbers.sh'`を足す
- e2eの印（`.config/e2e-quarantine.toml`）を変える: verifyに`sh scripts/check-e2e-quarantine.sh`を付ける（書式・重複・testの実在・上限を検査し、期限切れは警告だけ。CIも実行する。印の規則は[testの制約](testing.md)の「e2eの印」）
- pluginとバイナリのversionを変える: verifyに`sh scripts/check-plugin-version.sh`を付ける（検査の中身は[plugin integration](../design/plugin-integration.md)の「tagとversionの一致規則」）
- 対象が混ざるtaskは重い方の検証にする。

llvm-covとcargo testの重ね方:

- runtime（`src/`）を触るtaskは、verificationに`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`を含める。
- llvm-covをverificationに含めるtaskでは`cargo test --locked`をverificationに重ねない。llvm-covは`cargo test`と同じtest binary群を全部実行し1件でも落ちれば失敗するので（[testの制約](testing.md)の「test binary」）、両方を並べてもintegrateの直列の検証で同じtestが2回走る（約100秒）だけで検出力は増えない。
- llvm-covを含めないtask（docs・pluginの文書など）は、必要なら`cargo test --locked`をverificationに残す。

## pathsと軽い検証

変更の対象でverificationを軽くしてよい。条件は`--paths`で変えてよいパスを宣言すること（[ADR-0029](../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）。宣言外のパスを変えたrunはvalidatingで`needs_session`（`scope_violation`）になり、`integrate`もrebase後の差分を同じく検査して着地させないので、軽い検証のまま`src/`の変更が入ることはない。`--paths`を付けないtaskは制限されない。宣言外のパスが本当に要るときのworkerとplannerの手順はpluginの`dagq`の`reference/scope.md`の「What happens outside the paths」「Change the paths」が持つ。

## coverageの関門

- 登録済みのtaskの`cargo llvm-cov nextest --locked --fail-under-lines 80`と`cargo llvm-cov --locked --fail-under-lines 80`は書き換えず、同じ関門としてそのまま有効（ADR-0076決定4、[ADR-t828-1](../adr/2026-09-28-t828-1-coverage-gate-covers-the-workspace-with-workspace-flag.md)決定2。dagqのcoverageと全てのcrateのtestの成否は見るが、brokerのcrateの行はcoverageに数えない）。
- cargo-nextestが無いhost（[運用](operations.md)の「hostのツール」）では、plannerは旧コマンド（`cargo llvm-cov --locked --fail-under-lines 80`）でtaskを登録する。

## e2e

- runtimeのtaskに`--evidence e2e`を付けない。e2eの要否はvalidatingがrunの差分と`dagq.toml`の`[e2e] paths`から決めて`validation_finished`の`e2e_requirement`に記録し、要るrunにはreviewのpassの後にruntimeがhostでe2eを流す（[ADR-t963-1](../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定2・3、[ADR-t1233-2](../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)。仕組みは[Validation](../design/supervisor-lifecycle/validation.md)の「runtimeが流すe2e」と[Review](../design/supervisor-lifecycle/review.md)の「着地の前のe2e」。receiptの`e2e`のevidenceを求めないことも同じ）。
- `--evidence e2e`を明示したtaskは差分に依らずe2eが要るので、`[e2e] paths`の外でも実cmuxで確かめる必要があるとplannerが判断したときだけ付け、理由をdescriptionに書く。

## change

- `--change`はtaskが行う変更の種類を1つ宣言する（[ADR-t980-1](../adr/2026-09-29-t980-1-classify-runs-by-declared-change-and-diff-derived-area.md)）。値の集合は`dagq.toml`の`[tasks] changes`で、`add`のたびに付ける。集合の外の値とchangeの無いtaskを拒む仕組みはpluginの`dagq`の`reference/register.md`の「Task fields」（`--change`の箇条）、areaを着地の差分から求める仕組みは同じskillの`reference/scope.md`の「Recommended combinations」、areaの対応表は`dagq.toml`の`[areas]`が持つ。
- このrepositoryの7値の意味:
  - `feature`: 振る舞いや機能を足す・変える。ADRと実装を1 taskにするものも
  - `fix`: 決まった振る舞いと違う不具合を直す。不安定なtestを直すのは`test`
  - `refactor`: 振る舞いを変えずに構造を直す。ファイルの分割・名前の変更・重複の除去
  - `test`: testだけを足す・直す。不安定なtestの修正とtestのhelperを含む
  - `measure`: 数えて確かめる・測る。statsやkpiを読んで結果をdocsに書く測定と、測るための一時の計装
  - `docs`: 文書だけ。ADR・design・pluginのskill・AGENTS.md。実装を伴わない決定の記録
  - `config`: 設定と運用だけ。`dagq.toml`・CI・`scripts/`・toolchain・version
- 混ざるときは主な目的の1つを選ぶ（例: 不具合の修正にtestを足すなら`fix`、新しい機能のdocsを同じtaskで書くなら`feature`）。changeは検証を決めない（検証は上の推奨の組み合わせのとおり、変更の対象で選ぶ）。
- 作業時間の前後比較で読む層は[運用](operations.md)の「KPIの読み方と印」。

## plan reviewが当てはめる規則

この repository のplan review jobは、taskのverify・paths・evidence・changeと測定の書き方に次を当てはめる（読む文書の全体はAGENTS.mdの「plan review」）。

- verify・paths・evidenceは上の推奨の組み合わせに合い、changeは`dagq.toml`の`[tasks] changes`の7値のうちtaskの主な目的に合う1つであること。runtimeのtaskは`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`を持ち、llvm-covと`cargo test --locked`を重ねず、migrationを足すなら`check-migration-numbers.sh`を持つ。e2eの要否は差分からruntimeが決めるので、runtimeのtaskに一律の`--evidence e2e`は求めない。付いていれば`[e2e] paths`の外でも実cmuxで確かめる理由がdescriptionにあるかを見る。
- 測定のtask（changeが`measure`のtaskと、受け入れ条件に測定を含むtask）は、周回数（と交互に流すか）、表の列、値の計算式（何を何で割るか、待ちを引くときの区間）、証拠の所在（文書の節・CSV・script・コマンドと時刻の区切り）をacceptanceかdescriptionに書くこと（測定の形に当たらない項目、例えば1回だけ読む測定の周回数は、当たらない理由を書く）。条件の範囲を「同じ形のもの」で広げるtaskは、範囲を決めるgrepか一覧を書くこと。欠けていれば`revise`（workerはこれらを根拠に受け入れ条件の各項目を対応づける。[ADR-t1420-1](../adr/2026-10-03-t1420-1-worker-maps-each-acceptance-criterion-before-the-receipt.md)）。
