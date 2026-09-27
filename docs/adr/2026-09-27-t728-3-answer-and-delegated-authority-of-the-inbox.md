---
id: adr-t728-3
type: adr
title: inboxは全てのaskにanswerでき、dagq-recoverの手作業も人の言葉で代行してよいが、記録では人自身の操作とinboxの代行（delegated）を区別する。人しか出せない承認の強制は後のgoalにする
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
owners:
  - hisamekms
tags:
  - runtime
  - security
  - inbox
related:
  - adr-0022
  - adr-0047
  - adr-t728-1
  - adr-t728-2
---

# ADR-t728-3: inboxは全てのaskにanswerでき、dagq-recoverの手作業も人の言葉で代行してよいが、記録では人自身の操作とinboxの代行（delegated）を区別する。人しか出せない承認の強制は後のgoalにする

## Context

inboxは人に届くもの（askとattention）の唯一の窓口で、人の答えを`answer`で書き、人の指示で`dagq-recover` skillの手作業（`integrate`・`recover`・review無しの`ready`・`git push`など）を行う。[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)はinboxを信頼しないAI actorに分けたので、inboxのこれらの権限をどう扱うかを決める必要がある。セキュリティの計画は、人しか出せない承認（計画のI6: 着地の承認、破棄、認証、コストなど人が要る判断をAI actorが出せないこと）を不変条件に挙げている。

2026-09-27に人は、inboxは今までどおり全てのaskにanswerでき、`dagq-recover`の手作業も人の言葉で代行してよいと決めた。そのかわり記録の上で、人自身の操作とinboxが人の代わりに行った操作を区別する。

## Decision

1. **inboxの権限は今のまま。** inboxは全てのaskにanswerでき、`dagq-recover`の手作業も人の指示で代行できる。ADR-t728-1のpolicyはこれを許す（着地は[ADR-t728-2](2026-09-27-t728-2-landing-only-by-the-trusted-integrator.md)のとおりIntegratorへの依頼として）。
2. **記録で人自身と代行を区別する。** `DAGQ_ROLE`の無い呼び出し（人自身。ADR-t728-1のuser）とinboxの呼び出し（人の代わりの代行、delegated）を、answerと代行した操作のeventに区別して残す。記録はinboxのpromptや文から推さず、actorの型から決める。この区別もhost実行では助言的で、同じユーザーのプロセスはenvを偽れる（ADR-t728-1決定6）。
3. **人しか出せない承認（I6）はこの段では強制せず、後のgoalにする。** 理由: host実行ではinboxのsessionのterminalで人が打つ`!`のコマンドも`DAGQ_ROLE=inbox`を継ぐので、I6を強制すると、人は承認のたびにinboxとは別のterminal（`DAGQ_ROLE`の無いもの）を開く運用になる。今の運用（inboxで人に見せて答えを書かせる）を壊すので、承認の経路（別のterminal、署名、人の端末からの確認など）を決めてから強制する。
4. **answerはinboxとuserのもの。** 今の運用でanswerしないworker・job・observerのanswerは、ADR-t728-1決定7の強制としてpolicyで拒む。plannerの扱いは、今の運用で使う範囲を変えない（ADR-t728-1決定7）ようにpolicyの表（design）が決める。askの問いの主の欄の値の改名はgoal 48のtask 502が持ち、この段では変えない。

answerとeventの欄の名前（delegatedの表し方を含む）は[docs/design/](../design/)に書く。

## Alternatives

- **この段でI6を強制し、answerを人だけにする**: inboxのterminalの`!`もinboxの権限になり、人は別のterminalを開かないと答えられない。運用が壊れるので、承認の経路を決める後のgoalに回す。
- **区別を記録しない**: 事後に、人が決めたのかinboxが人の言葉を解釈して行ったのかが分からず、I6を強制する後のgoalの前提（今どれだけ代行しているか）も測れない。
- **inboxの代行を禁止する**: 人がCLIを直接打つ手間が増え、inboxを窓口にした運用（ADR-0022以来）と合わない。

## Consequences

- 運用は変わらず、記録から人自身の操作と代行の割合を読める。
- I6を強制するまで、inbox（信頼しないAI actor）が人の言葉を誤って解釈した操作は止まらない。これは既知の残りのリスクとして後のgoalで扱う。
