---
id: plans-index
type: design
title: Implementation plans
status: current
created: 2026-09-22
updated: 2026-09-28
last_verified: 2026-09-28
tags:
  - planning
---

# Implementation plans

計画文書は、実装の順序と完了条件を記録する。現在進行中の計画は `active`、完了した計画は `completed` に更新し、履歴として残す。個々のタスクの経過と状態はdagqのキュー（`dagq show ID`のrun履歴とreceipt）が正である。

- [Current plan](current.md)
- [Milestones](milestones.md)
- [ADR 0001〜0034の棚卸しと統合ADRの組](adr-inventory.md)
- [cargo llvm-cov nextestへの切り替え前後のintegrateのverifyの所要時間と遅いtest](nextest-measurement.md)（ADR-0076決定6の測定）
- [cargo llvm-cov nextestのtest段の後（一覧・profrawのmerge・report）の内訳](nextest-post-test-stage.md)（task 564）
- [sccache導入前後のintegrateのllvm-covの所要時間とhit率](sccache-measurement.md)（ADR-0049決定10の導入後の測定、task 460）
- [NEXTEST_TEST_THREADSとRUST_TEST_THREADSが4の期間の基準値と、8への変更後の比べ方](nextest-test-threads.md)（task 566の前後の比較）
- [スパイク：過去の run の再現で task の重さと手戻りの予測の担い手を比べる](spike-predictor-replay.md)
- [スパイク：Claude（claude -p）と Codex（codex exec）の非対話の worker の測定](headless-worker-spike.md)（goal 57、task 812）
