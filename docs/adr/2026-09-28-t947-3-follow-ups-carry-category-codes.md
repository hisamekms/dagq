---
id: adr-t947-3
type: adr
title: workerがreceiptのfollow_upsに種類の分類コードを付け、runtimeのplannerの判断（採用・不採用・重複・人への問い）と合わせて集計する
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
owners:
  - hisamekms
tags:
  - runtime
  - receipt
  - planning
  - measurement
related:
  - adr-0047
  - adr-t807-1
  - adr-t808-1
  - adr-t876-1
  - adr-t947-1
  - adr-t947-4
  - design-supervisor-lifecycle-receipt-and-session-exit
  - design-supervisor-lifecycle-draft-planners
  - design-supervisor-lifecycle-stats
  - plan-follow-up-kinds
---

# ADR-t947-3: workerがreceiptのfollow_upsに種類の分類コードを付け、runtimeのplannerの判断（採用・不採用・重複・人への問い）と合わせて集計する

## Context

workerはreceiptの`follow_ups`（`{"title", "description"}`の配列）に範囲の外で見つけた作業を書き、着地の後にruntimeがdraftにし、runtimeのplannerが採るか決める（ADR-0047決定16、[ADR-t807-1](2026-09-28-t807-1-bundle-drafts-of-one-piece-of-work-for-one-runtime-planner.md)、[ADR-t808-1](2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md)）。draftごとの結末（submit・cancel・重複・draftのまま）はADR-t807-1で記録されるが、follow_upが何の種類の作業かは記録に無い。

task 951の分析（[follow-up-kinds](../plans/follow-up-kinds.md)）では、follow_upのdraftは492件で、採用260・不採用180・重複50、着地は31%だった。種類ごとに結末が大きく違い、runtimeのplannerの後の採用率は不具合80%・testの不足93%に対し、判断の依頼13%・人の作業33%で、不安定なtestは41%が重複だった。減らす手（testの名前での重複の除去、判断の依頼と人の作業をdraftにしない、など）は種類ごとに違い、種類が無いと効果を測れない。

goal 64は、follow_upも起きたときに分類コードを記録すると決めた（2026-09-28、人とplanner）。

## Decision

1. **workerがfollow_upごとに種類の分類コードを1つ付ける。** receiptの`follow_ups`の各要素は、`title`と`description`に加えて種類のコードを持つ。迷ったら、そのfollow_upを片付けたときに何が変わるかで選ぶ。どれにも当たらなければ`other`にし、descriptionで説明する。付けるのは作業の中で見つけたworker自身である。
2. **重複や採否はコードにしない。** 重複・不採用は中身ではなく結末なので、種類のコードに入れない。結末はADR-t807-1の記録（draftごとの結末）と、cancelの理由の分類（[ADR-t947-4](2026-09-28-t947-4-cancel-carries-a-reason-code.md)）が持つ。種類と結末は別の軸として並べて読む。
3. **コードが欠けてもreceiptを拒まない。** receiptを拒むとrunのresumeが要り、種類の欠けに見合わない。runtimeは欠けを「未分類」、集合に無い値をその値のまま記録する。receiptのほかの検査（validationの受理）は変えない。
4. **runtimeが記録し、runtimeのplannerの判断と合わせて集計する。** runtimeはコードをfollow_upのdraftの出どころ（`follow_up_registered`とdraftの出どころの材料）に記録し、runtimeのplannerに渡すdraftの材料にも載せる。`stats`は種類ごとに、draftの件数、runtimeのplannerの判断（採用・不採用・重複・`planner_question`で人に問うた件数とその答え）、着地の件数、draftのままの時間を出し、`kpi`は種類ごとの採用率の系列を出す。コードの無い過去のfollow_upは書き換えず「未分類」として数える。
5. **一覧と定義はdesignが持ち、ADRなしに足し引きできる。** runのreviewの集合と同じ種類の問題（文書の取り残し、testの不足）には同じ名前を使う（[ADR-t947-1](2026-09-28-t947-1-review-verdicts-carry-reason-codes.md)決定2）。コードはlabelとして記録し、知らない値も読める（[ADR-t876-1](2026-09-28-t876-1-no-sqlite-check-constraints-until-schema-is-stable.md)決定3）。一覧・定義・receiptの欄名は[Receipt and session exit](../design/supervisor-lifecycle/receipt-and-session-exit.md#follow_upsの分類コード未実装)、runtimeのplannerの判断との突き合わせは[Draft planners](../design/supervisor-lifecycle/draft-planners.md#follow_upの種類と判断の集計未実装)が持つ。

## Alternatives

- **今のまま自由文だけにする**: 種類ごとの採用率は、全文を読み直して分類しないと出ない。
- **後から分析だけで分類する**: 読み手ごとに境（不具合か改善か、不具合か残りの範囲か）が揺れる。follow_upを書いたworkerが一番よく中身を知っている。
- **runtimeのplannerが付ける**: plannerは採否を決めるときにdraftを読むので付けられるが、採否の判断と種類の分類が同じ者になり、判断に合わせて分類が寄る。workerが付けたものをplannerの判断と突き合わせるほうが、種類ごとの採用率を偏りなく読める。
- **人が付ける**: follow_upのdraftは1日数十件で、人はその大半を読まない（runtimeのplannerが決める）。人に分類を求めると、人を通らないはずのdraftが人を待つ。

## Consequences

- receiptのJSONの`follow_ups`の要素に欄が加わる。古いreceiptと欄の無いreceiptはそのまま読める。
- 種類ごとに経路を変える手（判断の依頼をdraftにしない、testの名前で重複を除く、など）は、この決定に含めない。集計が揃った後に別のtaskとADRで決める。
- 実装（receiptの欄、記録、plannerへの材料、stats と kpiの出力、workerのprompt）はgoal 64の別のtaskで行う。着地するまで、follow_upsは今の形のままである。
