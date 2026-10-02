---
id: adr-t1310-1
type: adr
title: 着地待ちのrunのlanding recheckを、supervisorの着地だけでなく、mainが最後にrecheckを終えたmainと違うたびに走らせる（ADR-0068決定1をamends）
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
amends:
  - adr-0068 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
related:
  - adr-0068
  - adr-t1091-1
  - design-supervisor-lifecycle-landing-recheck
---

# ADR-t1310-1: 着地待ちのrunのlanding recheckを、supervisorの着地だけでなく、mainが最後にrecheckを終えたmainと違うたびに走らせる（ADR-0068決定1をamends）

## Context

[ADR-0068](0068-recheck-waiting-runs-after-each-landing.md)決定1は、着地待ちのrunのlanding recheckを、supervisorが着地させたrunの`run_integrated`だけで起動し、`integrate`を人が直接打った着地では走らせないと決めた（次にsupervisorが着地させたときに確かめられる）。また、handoffは予定だけのrecheckを捨て、起動し直したsupervisorは止まる前の着地を知らない。

そのため、人やinboxが直接`integrate`で着地させたとき、handoffや再起動の間にmainが動いたとき、dagqを通さないpushでmainが動いたときは、次にsupervisorが着地させるまで、待つrunが崩れたことに気づかない。人の答えを待つ間はsupervisorの着地が続かないこともあり、goal 39の受け入れ条件「mainが動いたら、reviewを受けて待つrunがまだきれいにrebaseできるかを確かめる」を満たさない（goal review 21）。

ADR-0068は番号付きの決定を6つ持ち、変えるのは決定1のきっかけだけなので、[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)に従ってamendsで直す。

## Decision

1. **recheckのきっかけを「supervisorの着地」から「mainが、最後にrecheckを終えたmainと違うこと」に広げる。** ADR-0068決定1の「`integrate`を人が直接打った着地では走らない」をやめる。
   - supervisorはloopの回ごとに、landing branchの先端を、最後に記録したrecheckの終わり（`landing_recheck_finished`）のmainと比べる。違っていて対象のrunがあれば、今の先端に対してrecheckを始める。比べる相手はqueueの記録から読むので、execの引き継ぎや起動し直しを越えて残り、止まる前に動いたmainも新しいsupervisorが最初の回で確かめる。同じmainに対しては2回確かめない。
   - 直接の`integrate`・handoffや再起動の間の着地・dagqを通さないpushを区別しない。先端がdagqのrunの着地なら、そのrunの着地としてrecheckを記録し、そうでなければ着地のrunの無いmainの動きとして記録する。
   - どの着地の後のrecheckかは、始めるときに確かめるmainで決める。supervisorの着地の予定はrecheckを始めるきっかけだけで、予定が残る間に別の着地がmainを動かせば、その着地を名指す。
   - 対象が無かったことで、そのmainを確かめ済みにはしない。後からrunが着地待ちになれば、mainが変わらなくても確かめる。
   - queueに生きているsupervisorが複数あっても、recheckはqueueで同時に1つだけ走る。supervisorの着地の後のrecheckもmainの動きのrecheckも同じ排他を取ってから始め、同じmainを誰が確かめたかは排他の中で読む。mainの動きを見るsupervisorを1つに選ぶことはしない（drainやhandoffのsupervisorが選ばれたまま、ほかのsupervisorがmainの動きを確かめられなくなるため）。
   - supervisor自身の着地を予定にすること、途中のmainの動きを最新の1件にまとめること、drain・handoffの回では新しく始めないこと、対象（ADR-0068決定1の対象のrun）、対象が無ければ何も記録しないことは変えない。
   - 確かめ方・見つかったときの扱い・askへの注記・resumeの数え方・記録（ADR-0068決定2〜6）は変えない。

## Alternatives

- **直接の`integrate`が自分でrecheckする**: `integrate`はsupervisorの外の短いprocessで、待つrunのcommandを流す専用のworktreeとtargetを持たず、recheckが同時に1つという規則も守れない。supervisorが居ない間に動いたmainも拾えない。
- **handoffの予定を次のprocessに引き継ぐ**: handoffは拾えても、再起動・直接の`integrate`・dagqを通さないpushは拾えない。最後に確かめたmainと比べれば、どのきっかけも1つの規則で扱える。
- **一定の間隔で待つrunを全部確かめ直す**: mainが動いていないのに確かめ直す分だけ無駄で、動いた直後に気づくのも遅れる。

## Consequences

- 人やinboxが直接`integrate`で着地させた直後、supervisorの次の回で、待つrunが崩れたことが分かり、崩れたrunはresumeへ回る。
- supervisorはloopの回ごとにlanding branchの先端を読む。mainが動かない間は、前に見た先端と同じならqueueを読まない。
- 着地のrunの無いmainの動きのrecheckは、記録の置き場所（どのrunに記録するか）と着地のrunの欄の値をdesignが決める。`stats`の`landing_rechecks`の数え方は変えない。
- 実装と[Landing recheck](../design/supervisor-lifecycle/landing-recheck.md)の更新はtask 1310が本ADRと同時に行う。
