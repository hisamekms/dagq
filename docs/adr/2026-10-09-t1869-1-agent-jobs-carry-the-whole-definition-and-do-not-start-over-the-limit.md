---
id: adr-t1869-1
type: adr
title: 1つのagentの定義を持つagentのjobは定義の節を切らず、定義が節の上限を超えたらそのjobを起動しない（ADR-t1566-1決定4・6をamends）
status: accepted
created: 2026-10-09
updated: 2026-10-09
accepted_on: 2026-10-09
amends:
  - adr-t1566-1 decision 4
  - adr-t1566-1 decision 6
owners:
  - hisamekms
tags:
  - runtime
  - review
  - eval
related:
  - adr-t1566-1
  - adr-t1728-1
  - adr-t1895-1
  - adr-t1453-1
  - design-supervisor-lifecycle-prompt
---

# ADR-t1869-1: 1つのagentの定義を持つagentのjobは定義の節を切らず、定義が節の上限を超えたらそのjobを起動しない（ADR-t1566-1決定4・6をamends）

## Context

[ADR-t1895-1](2026-10-06-t1895-1-review-stage-runs-agent-and-program-jobs-in-a-fixed-shape.md)で、reviewのagentはagentごとに1本の独立のheadless job（agentのjob）になった。
[ADR-t1728-1](2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)決定5で、agentのevalは本番のrunのreviewのagentのjobと同じ起動経路で、snapshotが返した定義をそのままpromptに持たせて測る。
evalが測るagentと本番がreviewに使うagentが同じであることが、evalの成績の意味の前提になる。

[ADR-t1566-1](2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)は、全てのheadlessのjobのpromptに節ごとと全体の上限を持たせ（決定4）、上限を超える入力でもpromptが上限に収まり省いた件数と読む方法が載ることをtestにする（決定6）。
これをagentのjobの定義の節に当てると、上限を超えた定義は切られ、切った定義のagentを測る・reviewに使うことになる。
それは測る対象を変え、ADR-t1728-1決定5の契約（定義をそのまま渡す）を破る。

ADR-t1566-1のAlternativesは「上限を超えたらjobを起動しない」を、見張りやreviewが黙って止まることを理由に退けた。
ここで決める範囲はそれに当たらない。
範囲は定義の節だけで、evalの周は人かplannerが定義を直して依頼し直せ、本番は`approve_landing`のaskで人のattentionに出て黙って止まらない。
今の定義は定義の節の上限に遠い。

## Decision

1. **適用の範囲。** snapshotが返した1つのagentの定義をpromptに持ち、1つのverdictを返すagentのjobだけに当てる。evalのagentのjobと、runのreviewのagentのjob（evalと同じ組み立ての関数で起動するもの）の両方を含む。全体のreviewのjob・plan review・observer・goal review・スループットの見直し・復旧job・runtimeのplannerは含まない。agentのjobでも定義以外の節（指示・材料の行）はADR-t1566-1の決定4〜6のまま（切って、省いたbyte数と読む方法を書く）。
2. **定義の節は切らず、要約せず、取りに行かせない。** evalが測るagentと本番がreviewに使うagentを同じものにするため。
3. **定義が定義の節の上限を超えたら、組み立ての関数は誤りを返し、そのagentのjobを起動しない。** evalはその周を起動せず、`definition_over_limit`をagent・定義のbyte数・上限とともにeventに記録し、成績を出さない（定義を直して依頼し直す）。本番のrunのreviewでは、そのagentの結果が揃わないものとして[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)決定6のpassにしない経路（`review_failed`と`approve_landing`のask。欠けたagentと理由に`definition_over_limit`を載せる）に渡し、passにも着地にもしない（同じ入力でのやり直しの細部は本番に広げるtaskが決める）。
4. **test。** 定義の節の上限ちょうどの定義が一字も切られずに載ってprompt全体が全体の上限に収まること、上限を超える定義で誤りになりpromptを作らないことを、ADR-t1566-1決定6のtestに代えてunit testにする。定義以外の節は決定6のtestのまま。

## Amendsの判断

ADR-t1566-1は番号付きの決定を7つ持ち、ここで変えるのは決定4（上限を超えたら決まった順で残す）と決定6（上限を超える入力でもpromptが上限に収まることをtestにする）に1種類のjobの1つの節の例外を足すことだけなので、丸ごと置き換えずamendsにする（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。
決定1〜3・5・7は変えない（agentのjobもstdinで渡し、読む経路はjobの権限で読めるものに限り、切った節に省いたことを書く）。

## Alternatives

- **定義を切る（ADR-t1566-1決定4のまま）**: 切った定義のagentを測り、reviewに使うことになり、evalの成績が本番のagentの成績でなくなる。
- **定義をファイルに置いて読ませる**: agentが全文を読んだかを確かめられず、定義の同一性の契約がpromptの外に出る。evalと本番で読み方が違えば測るものがずれる。
- **定義の節にも上限を持たない**: 全体の上限が効かず、ADR-t1566-1決定4の理由（費用と判断の質）が残る。上限は持ち、超えたら起動しないことで同一性と上限を両立させる。

## Consequences

- 定義の節の上限を超える定義はevalで測れず、本番のreviewもpassにならない。定義を短くするか上限を見直すtaskが要る（今の定義は上限に遠い）。
- 上限の値と理由、組み立ての関数の名前は、コードの定数とdoc commentが持ち、[Prompt](../design/supervisor-lifecycle/prompt.md)の「headlessのjobのprompt」がagentのjobの地図としてこの例外を指す。
