---
id: plan-team-version-owner-and-ask-routing
type: plan
title: チームの版のownerとaskの届け先の具体案（記録の形・繰り上げの待ちの値・配置の設定・CLI・eventの種類・優先度の検査の置き場）
status: active
created: 2026-11-03
owners:
  - hisamekms
tags:
  - architecture
  - authorization
  - priority
related:
  - adr-t3989-1
  - adr-t3985-1
  - adr-t3981-1
  - plan-team-version-identity
---

# チームの版のownerとaskの届け先の具体案

[ADR-t3989-1](../adr/2026-11-03-t3989-1-team-version-goal-owner-ask-routing-to-owner-delegate-and-on-call-admin-only-high-priority-and-shared-store-for-roadmap.md)の決定を実装するときの具体の候補を置く設計資料。
ADRは方針と境界だけを持ち、ここの名前・値・形は候補で、実装のtaskが選んで定義のそばのdoc commentへ移す。
まだ実装されていないので、`docs/design/`には書かない。人の登録と役割の形は[チームの版の識別の具体案](team-version-identity.md)を前提にする。

## 記録の形の候補（決定1・3）

- goalのowner: `goals.owner_issuer`・`goals.owner_subject`。チームの版では両方がNOT NULL、ローカルの既定ではNULL（hostのuserとみなす）。
- goalの無いtaskのowner: `tasks.owner_issuer`・`tasks.owner_subject`。goalを持つtaskでは常にNULLにし、読みは所属のgoalから引く。
- 代わりに答える人: `delegates(owner_issuer, owner_subject, delegate_issuer, delegate_subject, created_at)`。
- askの宛先: `ask_recipients(ask_id, issuer, subject, reason, added_at)`。`reason`は`owner`・`delegate`・`on_call`・`admin`。繰り上げは行を足すだけで、元の行を消さない。
- answerの記録: answerのeventに答えた人の`(issuer, subject)`と、その人の宛先の`reason`を足す。

## 繰り上げの待ちの値の候補（決定3・4）

配置ごとの設定に置く。値はどれも候補で、試行で人が決める。

| askの対象の効いている優先度（taskとそのrunはtaskに効いている優先度＝個別の指定か継いだgoalの値、taskに結びつかないgoalのaskはgoalの優先度） | ownerと代理から当番へ | 当番から管理者へ |
| --- | --- | --- |
| urgent・interrupt | 30分 | 1時間 |
| high | 4時間 | 8時間 |
| normal以下 | 24時間 | 48時間 |
| 運用のask（ownerを持つ対象なし） | — | 1時間 |

人が要る理由が`authentication`・`cost`のaskは、対象の効いている優先度に依らず上のurgentの行を使う案もある（止まっているrunが資源を持つため）。

```toml
[ask_routing.escalate_after]
urgent = "30m"
high = "4h"
normal = "24h"
operational = "1h"
```

## CLIの候補

- `dagq goal owner <goal> <member>`（今のownerか管理者）
- `dagq delegate add|remove|list <member>`（自分の代わりに答える人）
- `dagq ask list --mine`（自分が宛先のask。人ごとのinboxのwatchも同じ絞りで読む）
- `request add --priority high`を管理者でない人が打ったときの拒否の文: 上限（normal）と、管理者に頼めることを名指す。

## eventの種類の候補

- `goal_owner_changed`（前と後のowner、変えた人）
- `ask_escalated`（足した宛先と`reason`）
- `cross_owner_dependency_noticed`（依存する側と依存される側のtaskかgoal、plannerの推奨、優先度の逆転の有無）
- `priority_refused`（拒んだ段、出した人の役割）

## 検査の置き場の候補（決定6）

- 優先度の段の検査は、`StaticPolicy`のresourceの規則（ADR-t3985-1決定3の表の「優先度はnormalまで」）として、`task.write`・`goal.write`・requestの作成の手前で引く。
- 今のADR-t1975-1決定3の出どころの検査（AIのactorが`human`の値を変えない）はそのまま残し、段の検査はその前に引く。

## 実装の段の案（段6）

着地の順に並べる。どれもチームの版の配置（OIDCの発行元の設定）があるときだけ効き、ローカルの既定のtestが今のまま通ることを各taskの受け入れに入れる。

1. goalのownerの記録と継承: SQLiteとPostgresの両方のmigration、作るときのownerの決め方、goalを移したときの継承、ownerの付け替えと権限。
2. 代わりに答える人と、askの宛先の記録: 宛先の決め方（goalに属するtaskとrunはgoalのowner、goalの無いtaskとそのrunはtask自身のowner、どちらにも代理を足す。ownerを持つ対象の無い運用のaskは当番）、答えられる人の検査、answerの記録。
3. 繰り上げ: supervisorのループで待ちを過ぎたaskに宛先を足す、配置の設定の待ちの値、運用のaskの当番への届け、当番のいないときの管理者。
4. 人ごとのinboxの読みと通知: watchが自分宛てのaskだけを読み、繰り上げで宛先に入ったときにも通知する。
5. 優先度の権限: requestの欄・`set-priority`・`goal edit`の段の検査、AIのactorのnormalの上限とplannerの`scope`のask。
6. ownerをまたぐ依存の知らせ: plannerの推奨の記録と、依存される側のownerへのattention。
7. `docs/design/`の今の姿（askの宛先・優先度の出どころ・inbox）の更新は、それぞれの着地と同じ変更で行う。
