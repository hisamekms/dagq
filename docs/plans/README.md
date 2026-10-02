---
id: plans-index
type: design
title: Implementation plans
status: current
created: 2026-09-22
updated: 2026-10-02
last_verified: 2026-09-29
tags:
  - planning
---

# Implementation plans

計画文書は、実装の順序と完了条件を記録する。現在進行中の計画は `active`、完了した計画は `completed` に更新し、履歴として残す。個々のタスクの経過と状態はdagqのキュー（`dagq show ID`のrun履歴とreceipt）が正である。

- [Current plan](current.md)
- [Milestones](milestones.md)
- [ADR 0001〜0034の決定・後継ADR・designの対応表](adr-inventory.md)
- [cargo llvm-cov nextestへの切り替え前後のintegrateのverifyの所要時間と遅いtest](nextest-measurement.md)（ADR-0076決定6の測定）
- [cargo llvm-cov nextestのtest段の後（一覧・profrawのmerge・report）の内訳](nextest-post-test-stage.md)（task 564）
- [着地の検証でcoverageの計測をやめたときの短縮の見積もり](coverage-at-landing.md)（task 967）
- [sccache導入前後のintegrateのllvm-covの所要時間とhit率](sccache-measurement.md)（ADR-0049決定10の導入後の測定、task 460）
- [NEXTEST_TEST_THREADSとRUST_TEST_THREADSが4の期間の基準値と、8への変更後の比べ方](nextest-test-threads.md)（task 566の前後の比較）
- [遅いintegration testの時間が使われている待ちの内訳と、修正の候補の見積もり](slow-test-waits.md)（goal 68、task 975）
- [夜の人の答え待ちが着地を遅らせた量](night-human-wait-measurement.md)（goal 62、task 919）
- [スパイク：過去の run の再現で task の重さと手戻りの予測の担い手を比べる](spike-predictor-replay.md)
- [review と plan review の revise と concern で差し戻された理由の分類と、ラベルの定義案](review-sendback-reasons.md)（goal 64、task 945）
- [worker の問い（worker_question）の中身の分類と、ラベルの定義案](worker-question-topics.md)（goal 64、task 950）
- [receipt の follow_up の種類と runtime の planner の判断の分類と、ラベルの定義案](follow-up-kinds.md)（goal 64、task 951）
- [task の cancel の理由の分類と、ラベルの定義案](cancel-reasons.md)（goal 64、task 952）
- [スパイク：Claude（claude -p）と Codex（codex exec）の非対話の worker の測定](headless-worker-spike.md)（goal 57、task 812）
- [本番の queue での Claude の非対話の worker と対話の worker の比較と、既定を切り替えるかの推奨](headless-worker-measurement.md)（goal 57、task 821）
- [2026-09-26以降の本番のaskのkindごとの件数と、answerが推奨・見立てどおりだった割合](ask-outcomes-2026-09-26.md)（goal 42、task 451）
