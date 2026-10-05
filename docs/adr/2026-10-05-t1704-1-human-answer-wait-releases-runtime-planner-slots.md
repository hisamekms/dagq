---
id: adr-t1704-1
type: adr
title: 人の答えだけを待つ非対話のruntimeのplannerを終了させ、枠を空けて記録とanswerを新しいplannerに引き継ぐ
status: accepted
created: 2026-10-05
updated: 2026-10-05
accepted_on: 2026-10-05
amends:
  - adr-0047 decision 12
  - adr-0047 decision 13
  - adr-t1394-2 decision 3
  - adr-t1394-1 decision 4
  - adr-t1394-1 decision 7
  - adr-0047 decision 16
  - adr-0047 decision 19
  - adr-t807-1 decision 3
  - adr-t807-1 decision 5
  - adr-t1540-1 decision 5
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - headless
related:
  - adr-t1487-1
  - adr-t1404-1
  - adr-t1433-2
  - adr-t1433-3
  - adr-t1091-1
  - adr-t598-1
  - design-supervisor-lifecycle-draft-planners
  - design-supervisor-lifecycle-plan-planners
  - design-supervisor-lifecycle-finding-planners
  - design-supervisor-lifecycle-plan-review
---

# ADR-t1704-1: 人の答えだけを待つruntimeのplannerは枠を空け、answerで新しいplannerが続ける

## Context

人の`planner_question`への答えを待つplannerは、仕事のturnが終わってもruntimeのplannerの枠を持ち続ける。2026-09-28にはplanner 410が約5.5時間1枠をふさぎ、draft約20件とreviseが止まった（goal 63。枠を増やす当面の緩和を行った）。task 951の[測定](../plans/follow-up-kinds.md)でも枠待ちのp90は642分で、人の答え待ちが主因だった。

[ADR-t1487-1](2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)決定5のSpike起点の再計画も同じ枠を使う。人の不在で無関係な計画を止める必要はなく、plannerが居ないときに元の質問とanswerを持って新しいplannerを立てる経路は既にある。

## Decision

1. **人の答えだけを待つ非対話のplannerは終了させる。** 自分の未closeの`planner_question`が未回答で、turnが終わり、ほかに進められる仕事が無いときが対象である。未処理の続きの依頼・配送済みだが未読のanswer・providerの復旧待ちがあれば対象にしない。記録を残してから既存の終了の依頼を送り、wrapperの終了を確かめて行を閉じ、枠を空ける。終了の手順は[ADR-t1404-1](2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)決定3と[ADR-t1433-3](2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)決定1・3のまま、askは閉じない。ADR-0047決定13とADR-t1394-2決定3の「答えを待つあいだは終わらせない」をこの条件に限り変える。人が開いたplannerの部分は変えない。
2. **answerは既存の行き先に届ける。** 生きているplannerがまだ受け取れるなら次のturnへ、居なければ対象がまだ計画を要する場合に、枠の上限内で質問とanswerを持つ新しいplannerを立てる。終了の依頼を送ったplannerには新たに届けず、終了後の経路に残す。終了と回答が重なっても未読のanswerを捨てず、同じanswerを二重に届けない。draftの保留や対象が先に進んだ場合のaskのcloseなど、既存の結末は変えない。ADR-t1394-1決定7の届け方をそのまま使い、居ない側が通常になる。ADR-t1394-2決定7は、生きているplannerへの届け方として変えない。
3. **sessionをresumeせず、queueの記録から文脈を引き継ぐ。** 前のplannerは、決めたこと・決めかけの案・未決の点と理由・次にanswerで決めることを対象のnoteに残し、draftの編集も保存してからturnを終える。新しいplannerの初期promptに、元の質問・answer、前のplannerのnoteと編集済みのdraft、対象の依頼・finding・proposalと関連goalの最新の記録を載せる。収まらない材料は省いたことと読む方法を示す。session内だけの思考を唯一の引き継ぎにせず、既に決めた仕事をやり直さない。
4. **種類に依らず人だけの待ちを解く。** draftの束で一部だけ質問したなら、質問していないdraftの採否・依存・同梱を決め終えてから終了する。未回答のdraftは答えまで対象に戻さず、答えでは既存の束ね直しの経路で続ける。依頼とfindingも、質問を対象に結び付けて同じ条件で終了し、その対象へのanswerで新しいplannerが続ける。reviseを持つplannerも、質問以外に進められる修正を済ませ、出し直せない理由を記録して終了してよい。持ち主が閉じたreviseは既存の新しいruntimeのplannerへの配送に任せるが、未回答の問いで止まった同じ修正を答え無しで再起動し続けない。answerは対象のproposalの修正と一緒に引き継ぎ、別のplannerを二重に立てない。
5. **人だけの待ちで終了したことを、決めずに終わった回数に含めない。** 依頼（ADR-t1394-1決定4・7）、draft（ADR-0047決定16、ADR-t807-1決定3）、finding（ADR-0047決定19）の上限に共通して適用する。束の記録でも、人の答え待ちと決めずに終わった結末を区別する（ADR-t807-1決定5）。answerを持って立てたplannerは、決めずに終わったときだけ上限に数え、再び人だけの待ちで終了したなら数えない。再検討で立ったdraftのplannerにも待ちの除外を適用する（ADR-t1540-1決定5）が、再検討それ自体の算入と人・inboxによる上限超過の規則は変えない。質問を開いただけで、作業中の失敗・時間切れまで除外しない。通常の再起動は未回答のaskを越えず、answerによる再起動は既存どおり回数の上限を越えて運べる。
6. **応答しないplannerの知らせは変えない。** ADR-t1394-2決定3の「答えを待つあいだは数えない」は残す。人だけの待ちで終了したplannerには開いた行がなく、応答しない生きたplannerの検出対象ではなくなる。作業中のturnや、別の仕事を残してidleのままのplannerには既存の検出を適用する。
7. **対象は非対話の経路だけにする。** 対話の廃止は[ADR-t1433-2](2026-10-03-t1433-2-abolish-the-interactive-route.md)決定3（goal 92）が既に決めており、この変更に対話の終了・引き継ぎの仕組みを足さない。廃止まで残る対話のplannerの扱いは既存のままにする。ADR-t1487-1決定5の枠の共有と決定7の調査中の計画の上限・猶予は別の制御であり変えない。

## Alternatives

- **枠を持ち続ける**: sessionの文脈を保てるが、人がいつ答えるかに無関係なdraft・revise・再計画の進行が左右される。既存の新規plannerへの配送と永続した記録で続けられる。
- **時間の猶予を置いてから枠を空ける**: 短い待ちの再起動を減らせるが、人だけの待ちが確定している間も枠を浪費し、適切な猶予は人の在席に依る。turnの終わりと残る仕事で決める。
- **枠だけ数えずsessionを残す／前のsessionをresumeする**: 見えない待機processを抱え、provider固有のsessionの保持に継続を依存させる。新しいplannerが最新のqueueの記録から続ける方が再起動にも強い。
- **束の最初の質問で全件を止める／reviseの持ち主を残す**: 答え無しで決められる仕事まで止める。種類による例外より、進められる仕事を済ませて終了し、閉じた持ち主の既存の配送を使う。
- **答え待ちの終了やanswerを運ぶ起動を一律に上限に数える／全ての質問したplannerを除外する**: 前者は正当な人の判断待ちで使い切り、後者は失敗や時間切れの上限を無効にする。終了の理由と結末で分ける。
- **対話にも同時に実装する**: 廃止が決まった経路に新しい分岐と検証を足す費用に見合わない。

## Consequences

- 人の答え待ちはaskと記録が持ち、runtimeのplannerの枠は進められる計画に回る。新しいplannerの起動と材料の読み直しの費用が増える。
- 実装とintegration testは後続のtaskが行う。終了・配送の競合、束の部分的な質問、reviseの再起動、文脈と回数の引き継ぎを決定的に検査する。
- eventの種類・欄名・既定値はここで定めず、後続の実装がdesignに書く（ADR-t598-1決定2・3）。既存ADRは部分的な変更なので丸ごと置き換えない（ADR-t1091-1）。
