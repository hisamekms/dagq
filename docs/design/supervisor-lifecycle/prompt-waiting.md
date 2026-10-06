---
id: design-supervisor-lifecycle-prompt-waiting
type: design
title: "ダイアログ待ちの検知"
status: current
created: 2026-09-26
updated: 2026-10-06
last_verified: 2026-10-06
scope: runtime
related:
  - adr-t1433-2
  - design-supervisor-lifecycle
---

# ダイアログ待ちの検知（廃止）

worker の対話の run は task 1437 で廃止した（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。runtime は run の画面を読まず、既知のダイアログにも応答しない。`prompt_waiting`・`prompt_cleared`・`answer_prompt`・`dialog_answered`・`known_dialog_unanswered` の過去の event と ask は、履歴として引き続き読める。新しい worker の run にはこれらの検知・復旧 job・ask を起こさない。

## 既知のダイアログ

worker の固定キーによる応答は撤去した。人の planner と inbox の送信は [session-send](session-send.md) の `submit_input` を引き続き使い、ダイアログ上へ Enter を送り直さない。これらの画面の処理は task 1577・1442 の範囲である。runtime の planner には task 1441 から何も打たず、revise と answer は次の turn の依頼として書く。
