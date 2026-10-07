---
id: design-ci-failure-issues
type: design
title: Issue for failing CI on main
status: current
created: 2026-10-02
scope: operations
tags:
  - ci
related:
  - adr-t2034-1
  - adr-t1920-1
  - design-supervisor-lifecycle-ci-watch
  - design-stress-ci
  - design-linux-ci
---

# Issue for failing CI on main

main への push ごとの CI（`.github/workflows/ci.yml`）が落ちたら、GitHub の issue（label `ci-failure`）で人と planner に知らせ、次に通ったら閉じる。以前は失敗を届ける経路が無く、2026-09-28〜29 の main の直近 100 回のうち 49 回の failure が誰にも見られていなかった（task 1053、[docs/plans/coverage-at-landing.md](../plans/coverage-at-landing.md) の 6 章）。dagq の finding は CLI では observer しか書けないので（runtime が記録する経路は [CI watch](supervisor-lifecycle/ci-watch.md) が予定する。下の「supervisorのCIの見張りとの関係」）、[Stress CI](stress-ci.md) の `flaky-test` と同じく queue ではなく issue に残り、planner が issue から直す task を登録する。

## 部品

`.github/workflows/ci-failure.yml` の job `report` だけ。`ci.yml` の job は変えない。

## きっかけ

`workflow_run`（`workflows: [CI]`、`types: [completed]`、`branches: [main]`）で、`ci.yml` の実行が終わるたびに起動し、次の条件で動く。

- 実行の `event` が `push` のときだけ。`pull_request` の実行（head の branch が main でも）は issue を開きも閉じもしない
- 実行の結論（workflow 全体の `conclusion`）が `failure` なら開くか追記し、`success` なら閉じる。`cancelled`・`skipped` などでは何もしない（main の実行は concurrency で待っている古い実行が `cancelled` になる）
- `success` でも、実行の job（`gh run view <run> --json jobs`）に `conclusion` が `skipped` の job があれば、`cancelled` と同じく何もせず、「より新しい実行」の代わりにも選ばない（[ADR-t2034-1](../adr/2026-10-07-t2034-1-skip-rust-ci-jobs-on-docs-only-changes-and-do-not-read-skipped-runs-as-green.md)決定6）。
  docs だけの push は Rust の job を job の `if` で飛ばし（[Linux CI](linux-ci.md) の「docsだけの差分」）、その実行は Rust の成否を示さないため。
  job の名前の一覧は持たない（`ci.yml` で `if` を持つ job は Rust の job だけ）。
  doc の検査だけが落ちて開いた issue も、Rust を流して通る実行まで残る。
  supervisor の見張りも飛ばした success を緑と読まないが、`skipped` の job があるかではなく `[ci_watch]` で名指した job で決める（runtime は他の repository で普段から飛ぶ job を巻き込まないため。[CI watch](supervisor-lifecycle/ci-watch.md) の「実行の扱い」）

workflow 全体の結論で決めるので、`ci.yml` のどの job が落ちても（`checks` に限らず）開く。job 単位の `continue-on-error: true` の job が落ちても workflow の結論は `success` のままなので、その失敗だけでは開かない。`linux` job（[Linux CI](linux-ci.md)）は task 1238 で `continue-on-error` を外したので、その失敗で workflow の結論が `failure` になって開く。

通知の job を `ci.yml` の中に置いて全ての job を `needs` に持たせる作りは採らなかった。job 単位の `continue-on-error` の job の失敗が `needs.<job>.result` にどう出るか（`failure` か `success` か）に依って、`continue-on-error` を外す前後の振る舞いが変わりうるうえ、job を足すたびに `needs` を直す必要がある。`workflow_run` の結論は GitHub が workflow の成否として決めた値そのものなので、この扱いに依らない。

実行が終わった順と、この workflow の起動の順は揃うとは限らない。古い実行の結果で新しい実行の結果を上書きしないよう、起動した実行より後に作られ `success` か `failure` で終わった main の push の実行（`gh run list --workflow ci.yml --branch main --event push --status completed`）が既にあれば、起動した実行の代わりにその中で最も新しい実行の結論・commit・URL で決める（`skipped` の job がある `success` の実行は読み飛ばす）。
この workflow は concurrency の group で 1 本ずつ流し、group は待ちを 1 本しか持たないので、新しい実行の起動が待ちのまま cancel されることがあり、そのときは後から来た古い実行の起動がこの規則で新しい実行の結果を当てる。代わりに、新しい実行の起動も流れたときは同じ失敗を 2 回コメントすることがある（閉じるのは 2 回目には open な issue が無いので 1 回）。

## 開く・追記する・閉じる

- 落ちたとき: label `ci-failure` を（無ければ）作り、open な `ci-failure` の issue が無ければ title `CI on main is failing` で開き、あればコメントで追記する。open な issue は 1 つだけで、落ち続けるあいだ同じ issue にコメントが重なる
- 本文: commit（sha と subject の 1 行目）、実行の URL、落ちた job ごとの落ちた step の名前（`gh run view <run> --json jobs` の `conclusion` が `failure` の job と step）。`continue-on-error` の job が同じ実行で落ちていれば、それも落ちた job に並ぶ（workflow を落としたのはそれ以外の job）
- 通ったとき: open な `ci-failure` の issue に、通った commit と実行の URL をコメントして閉じる（`gh issue close --comment`）。open な issue が無ければ何もしない

## 権限

workflow の `permissions` は空（`{}`）で、job `report` だけが `actions: read`（実行の一覧と job・step を読む）と `issues: write` を持つ。`ci.yml` の job の権限は変えない（`ci.yml` は `permissions` を書かず、repository の既定のまま）。commit の message は自由な文字列なので、`run` の script に式で埋め込まず env で渡す。

## 確かめ方

`workflow_run` は default branch にある workflow の定義で動くので、この workflow は main に着地してから効く。手元では YAML として読めることと、`gh run list` / `gh run view --json jobs` と jq の式の形を、実際の main の実行に対して確かめた。

## supervisorのCIの見張りとの関係

[ADR-t1920-1](../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)決定8（見張りはtask 1921が実装した）。`dagq.toml`に`[ci_watch]`を書いたrepositoryでは、supervisorが同じmainのpushの実行を`gh`で読み、eventと既に落ちているtestの一覧と`ci_failure`のfindingをqueueに残し、修正taskはruntimeのfindingのplannerがplan reviewを通して作る（[CI watch](supervisor-lifecycle/ci-watch.md)）。

- このworkflowとissueは変えずに残す。supervisorが止まっている間も、見張りの無いrepositoryでも、GitHubの上で人に届く知らせだから。2つは別々に動き、互いを読まない。
- `[ci_watch]`を書いたrepositoryでは（`dagq ci failures`の`watch`が`available`でも`unavailable`でも）、plannerとinboxはissueから修正taskを登録しない。見張りが止まっている間はclaimと着地も止まり、戻れば見張りが同じ失敗を`ci_failure`のfindingにするので、issueから登録すると重なる。issueは人が読む知らせで、閉じるのは次に通った実行（`skipped` の job がある `success` は除く）。
- `[ci_watch]`の無いrepository（`disabled`）では、今までどおりplannerがissueから直すtaskを登録する。
- 判定の違い: issueはworkflow全体の`failure`だけで開くが、見張りは`timed_out`も赤とし、落ちたtestの組ごとにfindingを分ける。cancelされた実行と、Rustのjobを飛ばした`success`を緑と読まないのはどちらも同じ（見分け方は上の「きっかけ」）。
