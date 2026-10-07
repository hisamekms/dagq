---
id: adr-t2015-1
type: adr
title: 依頼のplannerが作りproposalに入れなかったdraftへの`planner_question`の答えは依頼の経路に乗せ、依頼が`open`でなくても答えを載せたplannerを依頼に立て直す（ADR-t1394-1決定7をamends）
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-t1394-1 decision 7
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - ask
related:
  - adr-t1394-1
  - adr-t1540-1
  - adr-t807-1
  - adr-t728-1
  - adr-t1091-1
  - design-supervisor-lifecycle-draft-planners
  - design-persistence
---

# ADR-t2015-1: 依頼のplannerが作りproposalに入れなかったdraftへの`planner_question`の答えは依頼の経路に乗せ、依頼が`open`でなくても答えを載せたplannerを依頼に立て直す（ADR-t1394-1決定7をamends）

## Context

[ADR-t1394-1](2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)決定7は、依頼のplannerの`planner_question`を依頼に結び付け、答えを依頼の生きているplannerに届け、居なければ依頼が`open`のときだけ答えを載せたplannerを立て、`open`でなければ届けずに閉じると決めた。依頼のplannerが`add`で作ってsubmitしなかったdraftを`--task`で聞くと、その問いは依頼に結び付かない。draftの経路はdraftの由来（runtimeやjobが登録した記録、revisitの時刻）・束・proposalで行き先を決めるので、どれも持たないこのdraftの答えは人の配送に落ち、draftには届ける相手が居ない。2026-10-06に、人の答えが10時間以上どこにも運ばれずdraftが残った（request 61が対策Aを選んだ）。

ADR-t1394-1は番号付きの決定を複数持ち、変えるのは決定7だけなので、[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)に従ってamendsで直す。

## Decision

1. **依頼のplannerが作ったdraftへの`planner_question`の答えは依頼の経路に乗せる。** 対象は、`draft`で、由来を持たず、取り消し以外のproposalに入っていないtaskで、それを作ったのがある依頼のruntimeのplannerであるもの。答えは、その依頼の開いているplanner（作ったplannerでも、同じ依頼の後のplannerでもよい）に届け、居なければ答えを載せた新しいplannerを依頼に立てる。
2. **依頼が`open`でなくても立て直す。** 提出済み・却下・上限の後でも、draftが残っていれば答えを載せたplannerを依頼に立てる。人の答えを運ばないまま残さないことが依頼の主旨で、残ったdraftを決めるのはplannerの仕事だから。上限は依頼の問いの答えと同じく、人の答えを載せるplannerだけが越えてよい。新しいplannerのpromptは依頼の答えを載せる形に、対象のdraftと、その答えをdraftに当てはめることを添える。
3. **作ったplannerはtaskの作成の記録の書き手で決める。** taskの作成のeventが記録するactor（ADR-t728-1）が`planner:N`で、そのplannerが依頼のために立ったruntimeのplannerなら、その依頼のdraftとする。plannerにdraftと依頼の対応を別に記録させない（記録が増えず、既にあるdraftにも効く）。
4. 変えないもの: `keep_draft`の答えは今どおり、開いているplannerが居ればそこに届け、居なければsupervisorが閉じる（plannerを立て直さない）。由来を持つdraft、proposalに入ったdraft、人や依頼の外のplannerが作ったdraft、findingと依頼への問い、workerの問いの経路。配送の取り合いの防止と、答えを待つ間のplannerと依頼の扱い（問いが閉じるまで答えを持たないplannerを依頼に立てない）は依頼の問いと同じにする。

## Alternatives

- **運ばれない答えの見張り（findingや催促）**: 落ちた後に気づくだけで、届け先を作らない。人が選ばなかった。
- **plannerのskillに「自分の依頼のdraftは`--request`で聞く」と書く**: 守られなければ同じ所に落ち、既に聞かれた問いを救わない。人が選ばなかった。
- **plannerがdraftを作るときに依頼との対応を記録する**: 新しい記録が要り、既にあるdraftには効かない。作成のactorで足りる。
- **依頼が`open`のときだけ立て直す（決定7のまま）**: 提出の後に残ったdraftの答えは閉じられ、draftは決まらずに残る。

## Consequences

- 依頼のplannerが残したdraftへの人の答えは、runtimeが届けるので、答えた時点で人の配送に回らない。
- 終わった依頼に後からplannerが立つことがある。そのplannerはdraftを決めるのが仕事で、依頼の状態は変えない（却下は`open`の依頼にしかできない）。
- 行き先の判定と記録は[Draft planners](../design/supervisor-lifecycle/draft-planners.md)の5と[Persistence](../design/persistence.md)の「answerの行き先」が持つ。
