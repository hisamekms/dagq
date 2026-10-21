---
id: adr-t2229-1
type: adr
title: hold-outとproductionの周はケースの集合の全体だけを流し、ケースのidの部分集合は人の再実行の指定のときだけ受ける（ADR-t1728-1決定7をamends）
status: accepted
created: 2026-10-21
updated: 2026-10-21
accepted_on: 2026-10-21
amends:
  - adr-t1728-1 decision 7
owners:
  - hisamekms
tags:
  - runtime
  - eval
related:
  - adr-t1728-1
  - adr-t1091-1
  - design-agent-eval
---

# ADR-t2229-1: hold-outとproductionの周はケースの集合の全体だけを流す（ADR-t1728-1決定7をamends）

## Context

ADR-t1728-1決定7は、hold-outを（agent、定義のdigest、ケースの集合のdigest）のキーごとに1回だけ流す。
evalの依頼はsplitのケースのidの部分集合を選べ、集合のdigestは流すケースから作るので、部分集合を変えるたびにキーが変わって初回になる。
そのため同じ定義で同じhold-outのケースを、部分集合を変えて何度でも流せ、1回の制限が抜けられる。
残るhold-outの実行は稀で、全体で流してよい。

## Decision

1. **held-outのsplitは集合の全体だけを流す。** hold-outとproductionの周では、ケースのidの部分集合を選ぶ入力を拒む。
   拒否は依頼の時に記録せずに理由を返し、記録に残った依頼にもsupervisorが周の開始で同じ判定を当てて、周を起動せずに拒否のeventを理由付きで記録する。
   devは今のまま部分集合を流せる。
2. **人の再実行の指定だけは例外。** 人の明示の再実行の指定（人と、人の言葉を受けたinbox。決定7の再実行と同じactor）の依頼は部分集合を受ける。
3. **集合の全体を選ぶ入力は拒まない。** queue側のproductionの集合を選ぶ入力は集合の全体を選ぶので、この拒否に当たらない。
   その集合に部分集合を重ねる依頼は、集合を全体で流すので再実行の指定があっても拒む。
4. 決定7のキーの定義と、evalのeventだけから読む判定は変えない。

## Rejected alternatives

- **ケースごとのキーにする。** 流したケースを1件ずつ数えれば部分集合でも抜けられないが、決定7のキーの定義を変え、evalのeventから読む既存の判定と記録を作り直すことになる。
  部分集合を流す必要は稀なので、拒否で足りる。

## Consequences

- 同じ定義のhold-outは、人の再実行の指定が無い限り集合の全体で1回だけ流れる。
- 部分集合で一部のケースだけを確かめたいときは、devに置くか、人の再実行の指定で流す。
