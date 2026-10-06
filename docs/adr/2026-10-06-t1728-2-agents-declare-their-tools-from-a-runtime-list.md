---
id: adr-t1728-2
type: adr
title: agentが使う道具は定義のfrontmatterでruntimeの一覧から名前を選んで宣言し、役割の権限を超える宣言を拒み、runtimeがproviderごとに独立のagentのjobの起動の設定へ変換する
status: accepted
created: 2026-10-06
updated: 2026-10-06
accepted_on: 2026-10-06
amends:
  - adr-t1453-1 decision 2
owners:
  - hisamekms
tags:
  - runtime
  - review
  - security
  - provider
related:
  - adr-t1453-1
  - adr-t1728-1
  - adr-t1895-1
  - adr-t1895-2
  - adr-t1470-1
  - adr-t1570-1
  - adr-t728-1
  - plan-review-agent-eval-spike
  - design-agent-eval
---

# ADR-t1728-2: agentが使う道具は定義のfrontmatterでruntimeの一覧から名前を選んで宣言し、役割の権限を超える宣言を拒み、runtimeがproviderごとに独立のagentのjobの起動の設定へ変換する

## Context

[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)決定2は、subagentの道具の許可を定義に書かせず、runtimeがreview jobと同じ読み取りだけに固定すると決めた。実装ではClaudeのreviewのsubagentは定義に依らず読む・探すだけの道具に固定され、Codexのsub-agentは読み取りだけのsandboxの中でshellを打てる。同じ定義でもproviderで使える道具が揃わない。人はrequest 35（goal 125）で、agentが使う道具を定義のfrontmatterで宣言できるようにすると決めた。

以前の予定（task 1728の具体化の途中）は、変換をClaudeの`--agents`のJSONとCodexの役割の設定（親のjobの中のsubagentの渡し方）に当て、Codexの親のjobに定義を渡せるかをtask 1476が確かめ、否定されればClaudeへの切り替え（`subagents_unsupported`）を保つ形だった。[ADR-t1895-1](2026-10-06-t1895-1-review-stage-runs-agent-and-program-jobs-in-a-fixed-shape.md)がrunのreviewのagentをagentごとに1本の独立のheadless jobにし（起動はtask 1903、親のjobの中のsubagentの仕組みの削除はtask 1904）、Codexでもsubagentを動かす能力が要らなくなったので、この予定は要らなくなった（task 1476は1903の重複としてcanceled）。

同じ具体化の途中では、(b)としてagentの定義のfrontmatterが決まった検査を名指し、supervisorがreviewの前に流して、違反ならagentを起動したうえで結果を`revise`に置き換えることも予定していた。これは[ADR-t1895-2](2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)のprogramのreview（landing branchのcommitから読むプログラムをagentの前に流し、落ちたらagentを起動せず差し戻す。宣言は`dagq.toml`のprogramの一覧）に一本化した。理由は、差し戻すと分かっているcommitに対して差し戻しの前にAIを動かさないことと、検査の宣言の置き場を1つにすることである。このADRはreviewの前の決まった検査を決めず、ADR-t1895-2を参照するだけにする。

## Decision

1. **宣言。** agentが使う道具は、定義のfrontmatterで、runtimeが持つ一覧から名前を選んで宣言する。任意のコマンドは書けない。
2. **役割の権限で拒む。** 役割の権限を超える道具の宣言（reviewなら読み取りを超えるもの）は拒む。拒んだ定義のagentは起動しない。
3. **providerごとの変換はruntimeが行い、独立のagentのjobの起動に当てる。** 変換の結果はADR-t1895-1の独立のagentのjobの起動の設定にする。Claudeはjob自身の起動の引数（許可と禁止の道具の一覧）、Codexはjobの設定にする。Codexでは今のreviewの読み取りだけのsandbox・jobのpermission profile・[ADR-t1570-1](2026-10-04-t1570-1-codex-run-review-distrusts-the-worktree-project.md)のworktreeのprojectの塞ぎ方を変えず、狭めるだけにする。ClaudeでもADR-t1470-1の設定の隔離とreviewの設定のdenyは変えない。これで同じ定義の道具がproviderに依らず揃う。
4. **当てないもの。** 変換は親のjobの中のsubagentの渡し方（Claudeの`--agents`のJSONの道具、Codexの役割の設定）には当てず、能力による切り替え（`subagents_unsupported`、ADR-t1453-1決定8）を前提にしない。全体のreviewのjobの道具はagentの定義を持たないのでこの宣言の対象にしない。
5. **evalも同じ変換を使う。** [ADR-t1728-1](2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)のevalは本番と同じ変換でagentのjobを起動する。

## Amendsの判断

ADR-t1453-1決定2の「subagentのtoolの許可は定義に書かせず、runtimeがreview jobと同じ読み取りだけに固定する」を、「定義が一覧から宣言し、runtimeが役割の権限で検査して変換する」に改める。reviewの道具が読み取りを超えないことは決定2で保つ。決定2の置き場はADR-t1728-1が改め、他の決定（1・3〜10）は変えないので、amendsにする。ADR-t1895-1の「Amendsの判断」は決定2の道具の固定を保つと書いたが、その道具の固定はこのADRが宣言と検査に改める（読み取りを超えないことは保つ）。ADR-t1895-1決定7（決定8の置き換え）・ADR-t1470-1・ADR-t1570-1は変えない。

## Alternatives

- **今のまま道具を固定する**: providerで使える道具が揃わず、agentごとに要る道具（例: 読むだけの検索）を足せない。
- **定義に任意のコマンドや許可の文字列を書かせる**: workerが定義を通じて権限を広げられ、providerごとの綴りが定義に漏れる。
- **親のjobの中のsubagentの渡し方に変換を当てる**: ADR-t1895-1で無くなる経路で、Codexではsubagentを動かす能力の確認と切り替えが要る。
- **決まった検査をagentの定義が名指し、agentを起動したうえで`revise`に置き換える**: 差し戻しの前にAIを動かし、宣言の置き場が2つになる。ADR-t1895-2のprogramのreviewに一本化した。

## Consequences

- 宣言の検査とproviderごとの変換の関数はtask 1873、独立のagentのjobの起動への適用はtask 1903、親のjobの中のsubagentの仕組み（`--agents`・Codexの役割の渡し方・`subagents_unsupported`）の削除はtask 1904が行う。ADR-t1895-1のConsequencesが道具の宣言の受け持ちとして挙げたtask 1875の番号は、task 1728の具体化（plan review）でこの3本の分担に決めた。
- frontmatterのkey・runtimeの道具の一覧の名前・providerごとの起動の設定の予定は[agent eval](../design/agent-eval.md)が持ち、今の姿は実装のtaskが[Review](../design/supervisor-lifecycle/review.md)に書く。
- reviewの前の決まった検査（programのreview）の宣言・終わりの区分・違反のときの行き先はADR-t1895-2と[Review](../design/supervisor-lifecycle/review.md)が持ち、このADRは持たない。
