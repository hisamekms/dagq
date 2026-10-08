---
id: plans-index
type: design
title: Implementation plans
status: current
created: 2026-09-22
tags:
  - planning
---

# Implementation plans

計画文書は、実装の順序と完了条件を記録する。計画の更新と状態（`active`・`completed`）の規則は[文書の規則](../development/documents.md)の「plans」が持つ。個々のタスクの経過と状態はdagqのキュー（`dagq show ID`のrun履歴とreceipt）が正である。

- [Current plan](current.md)
- [Milestones](milestones.md)
- [ADR 0001〜0034の決定・後継ADR・designの対応表](adr-inventory.md)
- [cargo llvm-cov nextestへの切り替え前後のintegrateのverifyの所要時間と遅いtest](nextest-measurement.md)（ADR-0076決定6の測定）
- [cargo llvm-cov nextestのtest段の後（一覧・profrawのmerge・report）の内訳](nextest-post-test-stage.md)（task 564）
- [着地の検証でcoverageの計測をやめたときの短縮の見積もり](coverage-at-landing.md)（task 967）
- [着地中にworkerのrunのCPUの優先度を下げたときの着地の検証の時間・workerの遅れ・loadの測定と、着地の枠を分ける効果の見積もり直し](landing-lane-cpu-share.md)（goal 65、task 961）
- [新しいrunのworktreeに温まったtargetをAPFSのcloneで入れたときのbuildの短縮の見積もり](worktree-seed.md)（goal 65、task 968）
- [build scriptのrerun-if-changedをworktreeに依らない形にしたときの、cloneしたtargetでのdagqのcrateのbuildの短縮の測定](build-script-rerun-paths.md)（goal 36、task 1359）
- [着地をまとめて検証する（batching）効果の、着地の枠を分ける案と合わせた見積もりと方式の候補](landing-batching.md)（goal 65、task 969）
- [landing recheckのきっかけを広げた（ADR-t1310-1）前後の本番のrecheckの件数・時間・commandの回数と、hostの負荷への影響](landing-recheck-trigger-measurement.md)（goal 139、task 1514）
- [sccache導入前後のintegrateのllvm-covの所要時間とhit率](sccache-measurement.md)（ADR-0049決定10の導入後の測定、task 460）
- [NEXTEST_TEST_THREADSとRUST_TEST_THREADSが4の期間の基準値と、8への変更後の比べ方](nextest-test-threads.md)（task 566の前後の比較）
- [遅いintegration testの時間が使われている待ちの内訳と、修正の候補の見積もり](slow-test-waits.md)（goal 68、task 975）
- [判断を unit test に移した着地（task 1412〜1416）の前後の本番の coverage の関門の test の時間と Summary](integration-to-unit-tests.md)（goal 68、task 1417）
- [tests/it の全 1,192 本の分類（境界・判断・代表あり・goal 92 で消える）と、it でないと担保できない test の見積もり](it-reduction.md)（goal 118、task 1706）
- [docs/design の4指標（総量と伸び・docs/design を変えた着地の割合・docs の衝突と claim の控え・道具の結果に占める docs）と docs だけの衝突の種類（M5）の定義と基準値](docs-slim.md)（goal 159、task 1946・1966）
- [過去の着地の差分に IT の対応表を当てた、絞った IT の時間と見逃しの測定と、全部流す閾値・共通のファイル・表の古さの上限](landing-it-selection.md)（goal 157、task 1924）
- [上限を入れた後の本番のobserverのpromptのbyte数と節ごとの大きさ、上限を見直すかの結論](observer-prompt-size.md)（task 1575）
- [broker の image の build のキャッシュの前後の、着地前の e2e と関門の e2e の時間・broker の e2e の完了順・e2e の待ちの測定と、broker の e2e を差分で絞る案の判断](broker-e2e-image-cache.md)（goal 93、task 1452）
- [夜の人の答え待ちが着地を遅らせた量](night-human-wait-measurement.md)（goal 62、task 919）
- [スパイク：過去の run の再現で task の重さと手戻りの予測の担い手を比べる](spike-predictor-replay.md)
- [review と plan review の revise と concern で差し戻された理由の分類と、ラベルの定義案](review-sendback-reasons.md)（goal 64、task 945）
- [worker が受け入れ条件を根拠と照合する変更の前の、run の review の差し戻しの基準値と、前後比較の script](acceptance-check.md)（goal 90、task 1422。後の区間は task 1423 の暫定と task 1537）
- [worker の問い（worker_question）の中身の分類と、ラベルの定義案](worker-question-topics.md)（goal 64、task 950）
- [receipt の follow_up の種類と runtime の planner の判断の分類と、ラベルの定義案](follow-up-kinds.md)（goal 64、task 951）
- [既存の open の goal に残る follow_up 由来の task と draft の所属の分類案](follow-up-membership-inventory.md)（goal 97、task 1512）
- [既存の open・draft の goal のラベルと優先度の初期値の案と、high 以上の goal に残る task の後回しの判定の案](goal-priority-inventory.md)（goal 106、task 1642）
- [task の cancel の理由の分類と、ラベルの定義案](cancel-reasons.md)（goal 64、task 952）
- [スパイク：Claude（claude -p）と Codex（codex exec）の非対話の worker の測定](headless-worker-spike.md)（goal 57、task 812）
- [本番の queue での Claude の非対話の worker と対話の worker の比較と、既定を切り替えるかの推奨](headless-worker-measurement.md)（goal 57、task 821）
- [非対話の worker の ask と待ちをゼロベースの形へ移す時期を判断する計測の項目・基準値・条件の案](zero-based-headless-readiness.md)（goal 86、task 1369）
- [非対話の worker の workspace のコスト（cmux の呼び出しの失敗・残った workspace・startup・待ちの間の workspace）の基準値と、background の wrapper に切り替えた後の評価のコマンドと戻す基準の案](headless-background-evaluation.md)（goal 89、task 1407）
- [runtime の planner の対話の期間の基準値と、非対話に切り替えた後の評価のコマンドと、対話に戻す基準の案](headless-planner-evaluation.md)（goal 87、task 1401）
- [スパイク：run の review job の中で review の subagent を Claude と Codex の非対話の呼び出しで動かせるか](review-subagents-spike.md)（goal 94、task 1453）
- [run の review の subagent receipt-evidence の選ばれ方・トークン・行き先と親の review との検査の重なりの測定と、選択肢と推奨](receipt-evidence-review-study.md)（goal 176、task 2108）
- [スパイク：reviewのagentをevalで測って改善する（外のrepository dagq-agent-evalのSpike〜MVPの結果・決めたこと・所見の行き先）](review-agent-eval-spike.md)（goal 125、task 1728）
- [AGENTS.md と plugin の dagq repository 固有の記述の棚卸しの表と、整理の前後の照合と役割ごとの読む量](agents-slim-inventory.md)（goal 94、task 1456・1463）
- [AGENTS.md の「作業中」「起動と停止」「着地と人の判断」から移した運用の規則の経緯](operation-rules-history.md)（goal 94、task 1458）
- [AGENTS.md の「変更後に必ず通す」「テストの制約」から移した手元の検証と test の規則の経緯](local-checks-history.md)（goal 94、task 1457）
- [2026-09-26以降の本番のaskのkindごとの件数と、answerが推奨・見立てどおりだった割合](ask-outcomes-2026-09-26.md)（goal 42、task 451）
- [ADR-t451-1の実装の着地の前後の、askのkindごとの件数・人の答え待ち・AIが決めた件数と、AIが決めたlandの後の手直し](ask-outcomes-after-adr-t451-1.md)（goal 34、task 1321）
