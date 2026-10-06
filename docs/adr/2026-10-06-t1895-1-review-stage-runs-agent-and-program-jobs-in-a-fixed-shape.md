---
id: adr-t1895-1
type: adr
title: runのreviewの段のjobはagentとprogramの2種で、起動・時間の上限・やり直し・記録・引き継ぎを共通にし、段の形（programを順に → agentを並列 → 機械の判定）は固定、中身の一覧は設定にする
status: accepted
created: 2026-10-06
updated: 2026-10-06
accepted_on: 2026-10-06
amends:
  - adr-t1453-1 decision 5
  - adr-t1453-1 decision 8
owners:
  - hisamekms
tags:
  - runtime
  - review
  - provider
related:
  - adr-t1453-1
  - adr-t1470-1
  - adr-t1570-1
  - adr-t1895-2
  - adr-t451-1
  - adr-t1063-1
  - adr-t1207-1
  - adr-t1091-1
  - adr-0027
  - design-supervisor-lifecycle-review
---

# ADR-t1895-1: runのreviewの段のjobはagentとprogramの2種で、起動・時間の上限・やり直し・記録・引き継ぎを共通にし、段の形（programを順に → agentを並列 → 機械の判定）は固定、中身の一覧は設定にする

## Context

今のrunのreviewは、1本の親のreview jobが基本のreviewと、`dagq.toml`の`[review.subagents.<agent>]`が差分のpathで選んだsubagentを中で動かし、1つのverdictに集約する（[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)決定5）。subagentの渡し方はproviderごとに違い、動かせないproviderからは`subagents_unsupported`で切り替える（決定8）。このため、AIの判断が要らない形式の検査もAIが見ており、subagentは親のjobに閉じて、時間の上限は親と共有し、結果の欠け（incomplete）とproviderの切り替えを別に扱う必要がある。request 40で人とinboxが、reviewの段のjobを「AI agent」と「プログラムの実行」の2種にすると合意した（goal 152〜154）。

ADR-t1453-1のAlternativesは「supervisorが独立の複数のreview jobを管理する」案を、jobごとの時間の上限・retry・切り替え・slot・eventと集約をruntimeが持つこと、基本のreviewの文脈が分かれることで退けた。次の3点から採り直す。

- **集約はすでにsupervisorが持つ。** 決定7で、supervisorは親の集約を信じず、親とagentの判定の1つずつから行き先を求めて最も重いものを取っている。親のjobの集約は判定に使われていない。
- **jobを共通にすれば費用は一度で済む。** 起動・時間の上限・やり直し・slot・events・引き継ぎを種類に依らない1つの経路にすれば、agentとprogramとそれ以後のreview（セキュリティのreviewなど）が同じ経路に載り、jobごとに作り直さない。
- **subagentも親の文脈を持たない。** subagentは親が渡すpromptと自分で読むfileだけで判断しており、独立のjobに分けても文脈は割れない。

## Decision

1. **jobの2種と共通の経路。** runのreviewの段のjobは、AIのproviderで動く**agent**のjobと、決まったプログラムを動かす**program**のjobの2種にする。起動・時間の上限（種類ごとに設定できる）・やり直し・jobの記録・supervisorの入れ替わりでの引き継ぎは、種類に依らず共通の経路を通る。programのjobの範囲と動かし方は[ADR-t1895-2](2026-10-06-t1895-2-program-reviews-are-fast-format-checks-read-from-the-landing-branch.md)が決める。
2. **段の形。** 段は次の順に固定する。
   1. programのreviewを設定の順に1本ずつ流す。1本でも落ちれば、agentのjobを1本も起動せずworkerに差し戻す。
   2. 全て通れば、agentのreviewを並列に起動する。全体のreview（今の基本のreview）のjobは常に1本で、差分のpathで選んだagentはそれぞれ1本ずつのjobにする。
   3. 判定は機械が行う。全てのagentのjobの判定がそろってから、supervisorが決定7の規則で行き先を求める。
3. **形は固定、中身は設定。** 設定にするのは中身（programの一覧とagentの一覧）だけで、段の順・並列か直列か・落ちたときの行き先は設定にしない。段のチェーン（順・分岐・条件・段の追加）を自由に定義する仕組みは作らない。
4. **AIの統合を判定に入れない。** agentのjobの判定を、AIのjob（全体のreviewを含む）がまとめて1つにする段を置かない。全体のreviewもagentと並ぶ判定の1つで、他のagentの判定を受け取らない。親のjobがagentの判定より軽くまとめる失敗（決定7が防いだもの）が戻らないようにするためである。
5. **引き継ぐもの。** ADR-t1453-1の次の決定はそのまま保つ。
   - 決定3（reviewするcommitの実際の差分のpathでagentを選び、選んだagentは重複なく全て必須）。
   - 決定4（設定と定義はlanding branchの着地したcommitのtreeから試行ごとに読み、run dirに写す）。programの一覧もここから読む。
   - 決定6（そろわなければpassにしない）。決定2-3の全てのjobの判定を数え、欠けた・失敗した・読めない判定は同じ入力で1回だけやり直し、なおそろわなければ`review_failed`と手動reviewにする。
   - 決定7（判定ごとに着地・差し戻し・人の判断に直し、最も重いものを適用する）。全体のreviewとagentの判定を同じ規則で並べる。
6. **agentのjobごとの設定の隔離。** 決定8を置き換えても、[ADR-t1470-1](2026-10-03-t1470-1-all-claude-run-reviews-load-no-setting-sources.md)と[ADR-t1570-1](2026-10-04-t1570-1-codex-run-review-distrusts-the-worktree-project.md)決定1〜4が決めたrunのreviewの起動の隔離は、分けた後のagentのjobの1本ずつ（全体のreviewのjobも、選んだagentのjobも）が引き継ぐ。Claudeのjobはsetting sourcesを空にしてauto memoryも読まず、Codexのjobはworktreeのprojectを`untrusted`にして`.codex/config.toml`と`.codex/rules`を読まない。repositoryの規則は、どちらもreviewのpromptが名指すinstructionsから読む。読み取りだけの許可とreviewの設定のdenyも同じに保つ。当てる範囲はADR-t1570-1決定2のとおりrunのreviewだけで、goal review・plan reviewなどの他のjobには広げない。
7. **providerの扱い（決定8の置き換え）。** agentはsubagentとして親のjobに渡さず、自分のjobとして起動する。provider ごとのsubagentの渡し方と、subagentを動かせないproviderからの`subagents_unsupported`による切り替えは無くなる。各agentのjobの行き先のproviderは、今のrunのreviewと同じ規則（`[roles.review]`、[ADR-t1063-1](2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)・[ADR-t1207-1](2026-09-30-t1207-1-codex-run-review.md)の使えないときの切り替えと控え、`--no-claude`の手動review）でjobごとに決める。

## Amendsの判断

ADR-t1453-1は番号付きの決定を10持ち、変えるのは決定5（親のjobで実行して集約する）と決定8（providerの扱い）だけなので、丸ごと置き換えずamendsにする（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。決定1・2（設定と定義の置き場）・3・4・6・7・9は保ち、決定10（設定の無いrepositoryは今のまま、旧バイナリが読めない`dagq.toml`のtableを足さない）は保つ。設定の無いrepositoryの段は全体のreviewのjob 1本になり、今と同じ形である。決定2の「runtimeが定義をproviderごとの渡し方に変え、道具を読み取りだけに固定する」は、渡す先がsubagentからagentのjob（定義の本文をそのjobのpromptにする）に変わるだけで、providerに依らない定義の形と道具の固定は保つ。決定9は、jobの出力がagentごとのjobのverdictになり、親のverdictの中のagentごとの結果が無くなるだけで、Task・receipt・queueのschemaを変えないことは保つ。

ADR-t1470-1（決定8・10をamendsした）とADR-t1570-1は変えない。ADR-t1470-1決定1の「必須のagentのあるreviewは`--agents`と`Agent`の許可を足す」は決定8の渡し方を写した文で、決定8の置き換えとともに渡すものが無くなるだけで、隔離の決定は決定6で全てのagentのjobに当てる。ADR-t451-1決定3（`concern`の扱い）とADR-0027決定2（verdictの3値と`review_finished`）も変えない。

## Alternatives

- **今のまま親のjobの中でsubagentを動かす**: AIの判断が要らない検査もAIが見て、subagentの時間の上限が親と共有され、providerごとの渡し方・切り替え・結果の欠けを別に扱い続ける。
- **段のチェーンを設定で自由に定義する**: 順・分岐・条件の組み合わせごとに、落ちたときの行き先・やり直し・passにしない規則を確かめる必要があり、workerに変えられない形を保つ範囲が広がる。今要る形は1つなので固定する。
- **全体のreviewやAIの統合のjobがagentの判定をまとめる**: 親がagentの差し戻しを軽い`concern`で包む失敗（決定7の例）が戻る。行き先は機械が判定ごとに求める。
- **programの検査をagentと並べて流す**: 形式の誤りが分かっているcommitにもAIを動かし、時間とtokenを使う。差し戻しの前にAIを動かさない。

## Consequences

- runtimeにreviewの段のjobの種類（agent / program）の型と共通の経路を足す（goal 152）。この段では今のrunのreviewはagentのjobとして今までと同じに動き、判定・行き先・eventは変えない。
- 決定2〜7への移行（programのreviewのfail-fastの接続、agentのjobの並列化、親のjobの集約とsubagentの渡し方の廃止、設定と定義の整理）は、goal 152〜154の後続のtaskが行う。programのjobのbackendのport（hostの外の実装）とeval（ADR-t1728-1）への接続はtask 1874、agentの定義の道具の宣言はtask 1875が持つ。移行が終わるまでは、ADR-t1453-1決定5・8の今の実装が動く。
- agentのjobの数だけslotとtokenを使う。並列の上限と時間の上限の値は実装のtaskが決め、[Review](../design/supervisor-lifecycle/review.md)が今の姿を持つ。
- 全体のreviewは他のagentの判定を見ないので、agentどうしで重なる指摘は差し戻しの理由に並ぶ。
