---
id: adr-t2022-1
type: adr
title: supervisorはinbox・plannerのhookの区間の終わりをcmuxの一覧でなく、plannerの行・wrapperのprocess・後のinboxのsessionから推定して閉じる（ADR-0048決定7をamends）
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
amends:
  - adr-0048 decision 7
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
related:
  - adr-0048
  - adr-t655-1
  - adr-t1433-1
  - adr-t1433-2
  - adr-t1404-1
  - design-provider-lifecycle
---

# ADR-t2022-1: supervisorはhookの区間の終わりをcmuxを使わずに推定して閉じる（ADR-0048決定7をamends）

## Context

[ADR-0048](0048-record-claude-sessions-by-kind-with-open-and-active-time.md)決定7の最初の項は、`SessionEnd`の来なかった`inbox` / `planner` / `runtime_planner`の区間を、supervisorが取り込み（決定8）のたびにworkspaceが`cmux workspace list`に居るかで見て、居なければ`inferred`で閉じると決めた。
[ADR-t1433-1](2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)決定1はsupervisorからcmuxを外したので、この項は文字どおりには成り立たず、実装からも消えた。
`SessionEnd`はworkspaceを閉じたときやClaude Codeの異常終了で来ないことがあり、閉じる契機が無いと区間は開いたまま残り、`stats`・`kpi`のsessionの時間を膨らませる。

## Decision

1. **ADR-0048決定7の最初の項の契機だけを、cmuxを使わない次の推定に置き換える。** supervisorは取り込み（ADR-0048決定8）の周回で、hookが記録した開いている区間のうち次のものを`reason: inferred`で閉じる。cmuxは呼ばない。
   - `runtime_planner`: その区間が名指すplannerの行が閉じた（`planner_closed`など）か、plannerのwrapperがbackgroundで動き、その記録したpidが死んだか、記録した起動時刻と違うprocessになった（[ADR-t1404-1](2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)決定2）。heartbeatの古さと、読めない起動時刻は証拠にしない。workspaceで動くwrapperは行が閉じるまで閉じない。plannerを名指さない区間は閉じない。
   - `planner`（廃止前に人が開いたもの）: その区間が名指すplannerの行が閉じた（[ADR-t1433-2](2026-10-03-t1433-2-abolish-the-interactive-route.md)決定5の`person_retired`など）。plannerを名指さない区間は、人のplannerの行が1つも開いていなければ閉じる（`dagq plan`は拒まれ、新しく開くことがない）。
   - `inbox`: 同じqueueに、より後に開いた別のsession_idのinboxの区間が開いている（workspaceが違い、hookの`next_span`で閉じなかったもの）。同じsession_idのresume・compactは同じ区間を続けるので閉じない。いちばん後の区間はこの契機で閉じない。
2. **閉じる時刻と取り込みはADR-0048決定7・8のsupervisorの推定の閉じ方のまま。** 時刻はtranscriptの最後のレコードの`timestamp`（読めなければ見つけた時刻）で、閉じるときに残りのturnを取り込む（[ADR-t655-1](2026-10-04-t655-1-hook-close-defers-transcript-intake-to-the-supervisor.md)決定1の「supervisorが推定で閉じる区間は閉じるときに取り込む」）。
3. **hookが閉じる区間の閉じ方は変えない。** `SessionEnd`、同じworkspaceの次の`SessionStart`、workspaceの無い同じkindの次の別sessionの`SessionStart`で閉じる区間は、ADR-t655-1決定1・3のまま（transcriptを読まずに先に閉じ、時刻はその報告の時刻、取り込み待ちの印を付け、supervisorの取り込みが後で1回だけ計測を記録する）。このADRはADR-t655-1をamendsしない。
4. **transcriptの更新の古さだけで閉じる時間の閾値は設けない。** 常駐のinboxの長いidleを終わりと取り違えないため（ADR-t655-1と同じ理由）。

ADR-0048決定7の残りの項（runとjobの区間の推定、時刻の決め方、`stats`の`inferred`）と、決定6・8は変えない。
推定の判断の入口は[provider-lifecycle](../design/provider-lifecycle.md#claude-sessionの区間)が持つ。

## Alternatives

- **amendsだけで推定をやめ、`SessionEnd`だけに任せる。** 採らない: 来なかった区間が開き続け、sessionの時間が膨らむ。
- **inboxの前の区間をhookの次の`SessionStart`で閉じる（ADR-t655-1決定3を広げる）。** 採らない: 人の承認の無い別の決定の変更になる。supervisorの推定なら、hookの閉じ方を変えずに済む。
- **planner・inboxのwrapperのheartbeatやtranscriptの古さで閉じる。** 採らない: backgroundのwrapperはpidと起動時刻で見る（ADR-t1404-1決定2）。古さは常駐のidleと区別できない。
- **cmuxの一覧を読み続ける。** 採らない: supervisorはcmuxを呼ばない（ADR-t1433-1決定1）。

## Consequences

- inboxを別のworkspaceで開き直すと、前のinboxの区間は次の取り込みの周回で閉じる。開き直さなければ、`SessionEnd`の来なかったinboxの区間は開いたまま残る。
- 同時に2つのinboxを別のsessionで開くと、前の区間は閉じられる（inboxは1つの前提）。
- 後のinboxをやめて前のsessionをresumeしても、前の区間は後の区間より古いので閉じられる。そのsessionの次の`SessionStart`が新しい区間を開き、後の区間は次の周回で閉じる。
- plannerの区間は、plannerの行を閉じる処理（abandonedの片付け・`person_retired`）が走った後の取り込みで閉じる。
