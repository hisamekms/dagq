---
id: adr-t655-1
type: adr
title: hookで閉じるinbox・plannerの区間はtranscriptを読まずに先に閉じ、残りのturnと計測はsupervisorの取り込みが閉じた後に区間ごとに1回だけ記録する。workspaceの無いhookの区間は同じkindの次の別sessionの開始で推定で閉じる（ADR-0048決定2・7・8・9をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-0048 decision 2
  - adr-0048 decision 7
  - adr-0048 decision 8
  - adr-0048 decision 9
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - plugin
related:
  - adr-0048
  - adr-t1091-1
  - design-provider-lifecycle
  - design-plugin-integration
  - design-supervisor-lifecycle-stats
---

# ADR-t655-1: hookで閉じる区間はtranscriptを読まずに先に閉じ、計測はsupervisorの取り込みに任せる（ADR-0048決定2・7・8・9をamends）

## Context

[ADR-0048](0048-record-claude-sessions-by-kind-with-open-and-active-time.md)決定8は、区間を閉じる記録（決定2の閉じるeventと同じ処理）が残りのturnを全部取り込むと決め、決定2・9は稼働時間を記録したかと読めない理由を閉じるeventに載せると決めた。inbox・plannerの区間を閉じるのはpluginの`SessionEnd` hookと、同じworkspaceの次の`SessionStart`で、どちらも閉じる前にtranscriptを読んでいた。

Claude Codeは`SessionEnd` hookに短い時間しか与えない。transcriptが長いかDBが混んでいると、hookが打ち切られて区間が開いたまま残り、次の`SessionStart`かworkspaceの消失（決定7）まで閉じない。cmuxのworkspaceを持たないhookの区間はworkspaceで辿れないので、決定7の推定でも閉じず、開いたままになる（task 655）。

## Decision

1. **hookが閉じる区間は、transcriptを読まずに先に閉じる。** hookの報告（`SessionEnd`と次の`SessionStart`）で閉じるinbox・plannerの区間は、transcriptを読まずに閉じるeventを書き、稼働時間がまだ取り込まれていないことだけをそこに印す。ADR-0048決定2・9の「閉じるeventに稼働時間を記録したかと読めない理由を載せる」は、hookで閉じる区間についてはこの印に改める。runとjobの区間、supervisorが推定で閉じる区間は、従来どおり閉じるときに取り込む。
2. **残りのturnと計測は、supervisorの取り込みが閉じた後に区間ごとに1回だけ記録する。** ADR-0048決定8のsupervisorの取り込みは、開いている区間に加えて、取り込み待ちの印を持つ閉じたhookの区間も対象にする。transcriptは書き込みのロックの外で読み、残りのturnを閉じた時刻で切って記録し、稼働時間・トークン数・model（読めなければ読めない理由）を、閉じるeventとは別の、取り込みを終えたことを示すeventに載せる。取り込みを終えた区間は二度と読まず、turnと計測を二重に数えない。閉じるeventとその時刻は書き換えない。決定8の「区間を閉じるときに残りのturnを全部取り込む」と「閉じるときに読めなければ稼働時間を記録しない印を閉じるeventに書く」は、hookで閉じる区間についてはこの取り込みに改める。読めなくても区間は閉じたままにし、取り込みに失敗した区間は次の取り込みで読み直す。1つの区間の失敗はほかの区間の取り込みを止めない。
3. **workspaceの無いhookの区間は、同じkindの次の別sessionの開始で推定で閉じる。** ADR-0048決定7に、workspaceの無い`SessionStart`が、workspaceの無い同じkindの開いている区間のうち別のsessionのものを推定で閉じる契機を足す。同じsessionの再開、別のkind、workspaceのある区間は閉じない。契機は経過時間ではなく次の別sessionの開始とし、常駐sessionの長いidleを終了と取り違えない。閉じる時刻はその開始の時刻とし、閉じるときtranscriptを読まないので、決定7の「transcriptの最後のレコードの時刻」には寄せない。workspaceの無い同じkindの同時sessionは区別できず、後の開始が前の区間を閉じる。次の開始が来なければ閉じる契機は無い。
4. **`stats`は取り込みを終えた区間の計測を閉じた区間の計測として読み、出力項目を変えない。** 閉じる前に書かれたものは読まない。取り込みが済むまでは、その区間は稼働時間の無い区間として数える。

eventの欄と値、取り込みの間隔、測った所要時間は[provider-lifecycle](../design/provider-lifecycle.md#transcriptと稼働時間)・[plugin-integration](../design/plugin-integration.md)・[stats](../design/supervisor-lifecycle/stats.md)が持つ。

## Alternatives

- **`SessionEnd`の中で読む時間に上限を付ける**: 上限を超えたら結局読まずに閉じることになり、計測が抜ける区間と抜けない区間の規則が2つになる。
- **transcriptの最終更新から一定時間で閉じる**: 常駐のinboxは長くidleでいるので、時間の閾値は生きているsessionを閉じる。
- **閉じるeventを取り込みの後に書き換える**: run_eventsはappend-onlyで、過去のeventを書き換えない。

## Consequences

- `SessionEnd` hookの所要時間はtranscriptの大きさに依らなくなる。DBの書き込み待ちは残る。
- hookの区間の稼働時間・トークン数・modelは、閉じてから次の取り込みまで遅れて`stats`に出る。supervisorが居なければ出ない。
