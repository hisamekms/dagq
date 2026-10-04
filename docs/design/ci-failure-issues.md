---
id: design-ci-failure-issues
type: design
title: Issue for failing CI on main
status: current
created: 2026-10-02
updated: 2026-10-05
last_verified: 2026-10-02
scope: operations
tags:
  - ci
related:
  - design-stress-ci
  - design-linux-ci
---

# Issue for failing CI on main

main への push ごとの CI（`.github/workflows/ci.yml`）が落ちたら、GitHub の issue（label `ci-failure`）で人と planner に知らせ、次に通ったら閉じる。以前は失敗を届ける経路が無く、2026-09-28〜29 の main の直近 100 回のうち 49 回の failure が誰にも見られていなかった（task 1053、[docs/plans/coverage-at-landing.md](../plans/coverage-at-landing.md) の 6 章）。dagq の finding は observer しか書けないので、[Stress CI](stress-ci.md) の `flaky-test` と同じく queue ではなく issue に残り、planner が issue から直す task を登録する。

## 部品

`.github/workflows/ci-failure.yml` の job `report` だけ。`ci.yml` の job は変えない。

## きっかけ

`workflow_run`（`workflows: [CI]`、`types: [completed]`、`branches: [main]`）で、`ci.yml` の実行が終わるたびに起動し、次の条件で動く。

- 実行の `event` が `push` のときだけ。`pull_request` の実行（head の branch が main でも）は issue を開きも閉じもしない
- 実行の結論（workflow 全体の `conclusion`）が `failure` なら開くか追記し、`success` なら閉じる。`cancelled`・`skipped` などでは何もしない（main の実行は concurrency で待っている古い実行が `cancelled` になる）

workflow 全体の結論で決めるので、`ci.yml` のどの job が落ちても（`checks` に限らず）開く。job 単位の `continue-on-error: true` の job が落ちても workflow の結論は `success` のままなので、その失敗だけでは開かない。`linux` job（[Linux CI](linux-ci.md)）は task 1238 で `continue-on-error` を外したので、その失敗で workflow の結論が `failure` になって開く。

通知の job を `ci.yml` の中に置いて全ての job を `needs` に持たせる作りは採らなかった。job 単位の `continue-on-error` の job の失敗が `needs.<job>.result` にどう出るか（`failure` か `success` か）に依って、`continue-on-error` を外す前後の振る舞いが変わりうるうえ、job を足すたびに `needs` を直す必要がある。`workflow_run` の結論は GitHub が workflow の成否として決めた値そのものなので、この扱いに依らない。

実行が終わった順と、この workflow の起動の順は揃うとは限らない。古い実行の結果で新しい実行の結果を上書きしないよう、起動した実行より後に作られ `success` か `failure` で終わった main の push の実行（`gh run list --workflow ci.yml --branch main --event push --status completed`）が既にあれば、起動した実行の代わりにその中で最も新しい実行の結論・commit・URL で決める。この workflow は concurrency の group で 1 本ずつ流し、group は待ちを 1 本しか持たないので、新しい実行の起動が待ちのまま cancel されることがあり、そのときは後から来た古い実行の起動がこの規則で新しい実行の結果を当てる。代わりに、新しい実行の起動も流れたときは同じ失敗を 2 回コメントすることがある（閉じるのは 2 回目には open な issue が無いので 1 回）。

## 開く・追記する・閉じる

- 落ちたとき: label `ci-failure` を（無ければ）作り、open な `ci-failure` の issue が無ければ title `CI on main is failing` で開き、あればコメントで追記する。open な issue は 1 つだけで、落ち続けるあいだ同じ issue にコメントが重なる
- 本文: commit（sha と subject の 1 行目）、実行の URL、落ちた job ごとの落ちた step の名前（`gh run view <run> --json jobs` の `conclusion` が `failure` の job と step）。`continue-on-error` の job が同じ実行で落ちていれば、それも落ちた job に並ぶ（workflow を落としたのはそれ以外の job）
- 通ったとき: open な `ci-failure` の issue に、通った commit と実行の URL をコメントして閉じる（`gh issue close --comment`）。open な issue が無ければ何もしない

## 権限

workflow の `permissions` は空（`{}`）で、job `report` だけが `actions: read`（実行の一覧と job・step を読む）と `issues: write` を持つ。`ci.yml` の `checks` と `linux` の権限は変えない（`ci.yml` は `permissions` を書かず、repository の既定のまま）。commit の message は自由な文字列なので、`run` の script に式で埋め込まず env で渡す。

## 確かめ方

`workflow_run` は default branch にある workflow の定義で動くので、この workflow は main に着地してから効く。手元では YAML として読めることと、`gh run list` / `gh run view --json jobs` と jq の式の形を、実際の main の実行に対して確かめた。
