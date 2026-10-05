---
id: adr-t807-1
type: adr
title: 同じきっかけ（同じrunのreceipt・同じgoal review・同じwithdraw）で作られたdraftを1つのruntimeのplannerにまとめて渡し、まとめ方とdraftごとの結末と出どころを記録する（ADR-0047決定16をamends）
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
amended_by:
  - adr-t1540-1
amends:
  - adr-0047 decision 16
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - follow-up
related:
  - adr-0047
  - adr-t598-1
  - adr-t808-1
  - design-domain-model
  - design-supervisor-lifecycle-draft-planners
  - design-persistence
---

# ADR-t807-1: 同じきっかけ（同じrunのreceipt・同じgoal review・同じwithdraw）で作られたdraftを1つのruntimeのplannerにまとめて渡し、まとめ方とdraftごとの結末と出どころを記録する（ADR-0047決定16をamends）

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定16は、runtimeやjobが作ったdraftを「1件ごとに」runtimeが立てるplannerに採否を決めさせると決めた。これはgoal 29のconstraintsにある人の決定（2026-09-25）「integrateが作ったfollow_upのdraft 1件ごとに、supervisorがplannerを1つ立てる」を引き継いだものである。

この形では、1つのrunのreceiptがfollow_upsを3件出すとplannerが3つ立ち、それぞれが同じ元のtask・receipt・goalを読み直し、互いの重複や依存を見ずに別々にsubmitする。どのdraftが何に起因するか（元のtask・run・receiptの何番目か）も、`draft_origins`の行とcontextの一行と元のrunのeventにしか無く、`dagq show`のtaskから構造として読めない。

2026-09-27に人がplannerに依頼した: 「followupの検証をplannerがする時、作成タイミングが同じfollowupは1つのplannerで処理するようにできる？ 記録もとる」「何に起因するfollowupなのか…合わせて記録するようにしてほしい」。

## Decision

1. **同じきっかけで作られ、同時に待っているdraftを1つの束にし、束ごとにruntimeのplannerを1つ立てる。** これはgoal 29のconstraintの人の決定（2026-09-25）「follow_upのdraft 1件ごとにplannerを1つ」を、2026-09-27の人の決定（上の依頼）で置き換えたものである。ADR-0047決定16の「1件ごとに」を「束ごとに」と読み替える。plannerは束の中の重複・依存・1つのproposalへの同梱を1つのsessionで判断し、draftごとに採用・不採用・判断できない（`planner_question`）を選ぶ。
2. **「作成タイミングが同じ」を、同じ1回の処理が作ったdraftと読む。** 人の言葉「作成タイミングが同じ」を根拠に、plannerは次のように解釈した: follow_upは同じrunのintegrate（元のrun）が登録したもの、goal_gapは同じgoal reviewが作ったもの、reopenedは同じwithdrawが戻したもの（reopenしたplan reviewのproposalが同じもの）。作成時刻の近さでは束ねない（別のrunのfollow_upsを混ぜないため）。材料にきっかけが無いdraftは1件で1つの束にする。
3. **上限と回数の数え方は今の意味を保つ。** runtimeのplannerの同時の数の上限は束1つを1と数える。1件のdraftに立てるplannerの3回の上限は、今までどおりdraftごとに数える（そのdraftを含む束のplannerの数）。plannerが決めずに終わったdraftは次の機会に再び対象になり、残りと、その間に同じきっかけで増えたdraftで束を作り直す。3回に達したdraftは束から外して人に知らせる。
4. **人を経ない採用の上限と`planner_question`はdraftごとのまま変えない。** 閉じたgoalや深い段のfollow_up（[ADR-t808-1](2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md)）は、束の中でも人の`adopt`が無ければsubmitできない。`planner_question`はdraftごとに開き、answerはそのdraftを持つ生きている束のplannerに届ける。
5. **束とdraftごとの結末と出どころを記録し、読めるようにする。** 束（plannerとdraftの対応、きっかけの種類と値）と、plannerが終わったときのdraftごとの結末（submitしたproposal、cancel、重複先、`keep_draft`、決めずに終わった）をqueueに残し、draftの出どころ（種類、材料、元のtaskとrun、receiptの何番目か、束）を`show`のtaskに構造で出し、元のtaskから、そのrunのreceiptが出したdraftと今の状態を逆にたどれるようにする。出どころのきっかけをplannerを立てた記録・採用・cancel・結末の記録に載せ、元のrunから各draftの結末まで引けるようにする。人が`add`したtaskの出どころは無い。

表・列・eventのkindと欄名・migrationの番号・promptの文面・statsの欄は[Draft planners](../design/supervisor-lifecycle/draft-planners.md)・[Domain model](../design/domain-model.md#draft-planners)・[Persistence](../design/persistence.md)が持つ。

## Alternatives

- **作成時刻の近さで束ねる**: 同じ時刻の近くに着地した別のrunのfollow_upsが混ざり、元のtaskとreceiptが1つに決まらない。
- **draft 1件ごとのまま、promptに同じきっかけの他のdraftを見せる**: 同じ材料を読み直す手間が残り、重複と依存を別々のsessionが同時に判断して食い違う。
- **3回の上限を束ごとに数える**: 決めずに終わるdraftが1件あるだけで、同じ束の他のdraftも人に回ってしまう。

## Consequences

- 1つのrunのreceiptが複数のfollow_upsを出しても、runtimeのplannerは1つで、束の中の重複はcancel（重複先つき）に、依存は依存の追加になり、採用したものは1つのproposalで出せる。
- 同じきっかけのdraftが束のplannerの作業中に増えたら、そのplannerが終わるまで待ち、次の束に入る。
