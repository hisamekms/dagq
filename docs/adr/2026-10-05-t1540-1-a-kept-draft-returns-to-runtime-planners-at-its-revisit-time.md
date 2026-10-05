---
id: adr-t1540-1
type: adr
title: draftに再検討の時刻を持たせ、時刻が来たら（keep_draftで残したdraftも人がaddしたdraftも）runtimeのplannerの対象に戻して前回の判断を初期promptに載せる。付けるのはruntimeのplanner・人・inboxで、時刻の無いkeep_draftは今までどおり依頼を待つ（ADR-0047決定13・16、ADR-t1394-1決定8、ADR-t807-1決定1・3をamends）
status: accepted
created: 2026-10-05
updated: 2026-10-05
accepted_on: 2026-10-05
amended_by:
  - adr-t1704-1
amends:
  - adr-0047 decision 13
  - adr-0047 decision 16
  - adr-t1394-1 decision 8
  - adr-t807-1 decision 1
  - adr-t807-1 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - follow-up
related:
  - adr-0047
  - adr-t1394-1
  - adr-t807-1
  - adr-t808-1
  - adr-t451-1
  - adr-t598-1
  - design-supervisor-lifecycle-draft-planners
  - design-persistence
  - design-domain-model
  - design-authorization
---

# ADR-t1540-1: draftに再検討の時刻を持たせ、時刻が来たらruntimeのplannerの対象に戻して前回の判断を初期promptに載せる

## Context

runtimeのplannerが`planner_question`に`keep_draft`と答えられたdraftは、[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定16の「対象」（`keep_draft`で残されていない）から外れ、[ADR-t1394-1](2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)決定8のとおり、人の言葉を受けたinboxが参照付きの計画の依頼を記録するまで待つ。人が`add`で作ったdraftは、出どころが無いのでそもそも対象にならない。

「時間が経ったら埋めてsubmitする」つもりで残したdraftを決めた時刻に拾い直す仕組みがqueueに無く、inboxのHANDOFFやmemoryの約束に頼っていた（2026-10-03の人の依頼）。例: draft 1537はplanner 815がask 358の答え`keep_draft`の後のnoteに「早くても2026-10-04の昼（UTC）に埋めてsubmit」と書いて閉じたが、その時に動く主体が居ず、goal 90は1537がdraftのままでは閉じない。人がaddした1374・1403・1409・1452も同じ形で待つ。

## Decision

1. **持ち方: draftごとに再検討の時刻を1つ持つ。** 時刻（と、そのときに見るもののnote、付けたactor）をdraftごとに1つqueueに持ち、付け直せば置き換える。条件（「task Nの着地の後」など）は持たない（代替案）。
2. **付ける経路と権限: runtimeのplanner・人・inboxが付け・変え・外す。** runtimeのplannerは`keep_draft`で残すときに自分で時刻を付けて終われる（ADR-0047決定13をamends）。付けた時刻は`planner_question`のanswerを待たずに効き、推奨が`keep_draft`で決められるなら聞かずに時刻付きで残してよい（[ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)の基本方針）。人とinboxはCLIで付け・変え・外す。worker・observer・jobは拒む。これは開始前のtaskを変える操作なので、新しいcapabilityを作らず、同じroleの集合を持つtaskの変更の権限に含める（plannerは開始前のtaskだけ、が今の規則のまま効く）。plannerは上限に達したdraft（決定5）には付けられない。
3. **時刻を過ぎたdraftはruntimeのplannerの対象に戻る（ADR-0047決定16をamends）。** 「対象」の条件のうち「`keep_draft`で残されておらず」と「人が`add`で作ったdraftは対象にならない」に、再検討の時刻を過ぎたdraftの例外を足す。出どころの無い人のdraftは出どころ`revisit`として扱う（人のdraftであることは変えず、`show`の出どころはnullのまま）。まだ来ない時刻を持つdraftは、`keep_draft`の答えが無くても対象から外す（時刻付きで残したplannerの後に次のplannerがすぐ立たないように）。`keep_draft`の項の行き先に「時刻を付けたなら、その時刻に対象に戻る」を足す。plannerを立てたら時刻は使い切り（1回だけ。eventに残す）、そのplannerがまた残すなら新しい時刻を付けさせる。初期promptに時刻・付けたactor・noteと、前回の判断（そのdraftの`planner_question`の質問・推奨・answerと、draftのnoteの本文）を載せる。
4. **束は元のきっかけで束ね直さず、task 1件の束にする（ADR-t807-1決定1をamends）。** 再検討のdraftの同じきっかけのdraftは、もう別の結末になっているか別の時刻を持つ。再検討はそのdraftの判断のやり直しなので、1件で1つの束にする。
5. **上限との関係（ADR-t807-1決定3をamends）。** 再検討で立つplannerも、そのdraftのplannerの1回と数える（3回の上限の意味を保つ）。ただし人・inboxが付けた再検討は、上限に達していても1回立てる（人のanswerを運ぶときと同じく、人の判断を経たものとして）。plannerが付けた時刻は上限を越えさせない。[ADR-t808-1](2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md)の人を経ない採用の上限は変えない（再検討のplannerもruntimeのplannerなので効く）。
6. **時刻の無い`keep_draft`は今の振る舞いを変えない。** 対象から外れたまま、ADR-t1394-1決定8の依頼を待つ。既定の期限を付けると、判断の材料が増えていないdraftにplannerを周期的に立て、枠とtokenを使うため。見分けは`show`で行う（再検討の時刻が無く`keep_draft`のanswerがある）。`status`の一覧には足さない（`status`は今の手当てが要るものを出し、時刻の無い`keep_draft`は人の言葉を待つだけなので）。
7. **依頼の経路と並べる（ADR-t1394-1決定8をamends）。** `keep_draft`で残したdraftの行き先（inboxが参照付きの依頼を記録するか、人のterminalで決める）は変えずに残し、再検討の時刻の経路を並べて足す（置き換えない）。同じdraftに両方が来たら同時に2つのplannerを立てない: 開いている（`open`の）依頼が参照するdraftは、依頼のplannerが終わるまで再検討のplannerを立てず（時刻は残り、依頼の後もdraftならその後に立つ）、再検討（とほかの束）のplannerが開いているdraftを参照する依頼は、そのplannerが終わるまで依頼のplannerを立てない。
8. **記録。** 付けた・変えた・外した・時刻が来て対象に戻した（使い切った）ことを、actor付きのeventに残し、`show`に再検討の時刻（使ったならいつ、どのplannerか）を出す。

表・列・eventの種類と欄・CLIの綴り・promptの文面は[Draft planners](../design/supervisor-lifecycle/draft-planners.md)・[Persistence](../design/persistence.md)・[Domain model](../design/domain-model.md#draft-planners)・[Authorization](../design/authorization.md)が持つ。

## Alternatives

- **条件（task Nの着地の後など）で再検討する**: 条件はほとんど依存で表せ（draftに依存を足してsubmitする）、今待っている5件は時刻で足りる。条件の評価をsupervisorに足す手間に見合わない。要るときに足す。
- **時刻の無い`keep_draft`に既定の期限を付ける**: 決定6の理由で退けた。
- **再検討のdraftを元のきっかけで束ね直す**: 同じrunの他のdraftは既に決まっているか別の時刻を持ち、束ね直しても一緒に判断するものが無い。
- **再検討のplannerを3回に数えない**: plannerが時刻を付け続けると上限が効かず、決めきれないdraftが人に上がらない。
- **新しいcapabilityを作る**: 許すroleと拒むroleが開始前のtaskの変更と同じで、表を分けると写す文書が増えるだけになる。
- **依頼の経路を置き換える**: 時刻を決められないdraft（人の判断を待つもの）の行き先が無くなる。

## Consequences

- inboxのHANDOFFやmemoryに書いていた「この時刻にsubmitさせる」約束をqueueが持ち、時刻が来ればruntimeのplannerが前回の判断を読んで決め直す。
- 人がaddしたdraftも、人かinboxが時刻を付ければruntimeのplannerが1回扱う。時刻を付けない人のdraftは今までどおり誰も扱わない。
- 時刻の無い`keep_draft`のdraftは今までどおり人の言葉を待ち、`status`には出ない。
