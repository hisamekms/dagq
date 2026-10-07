---
id: plan-local-checks-history
type: plan
title: AGENTS.mdの「変更後に必ず通す」「テストの制約」から移した手元の検証とtestの規則の経緯
status: completed
created: 2026-10-03
owners:
  - hisamekms
tags:
  - testing
  - documentation
related:
  - adr-t1453-2
  - development-local-checks
  - development-testing
  - development-task-registration
  - plan-agents-slim-inventory
  - plan-load-spike-2026-09-27
---

# AGENTS.mdの「変更後に必ず通す」「テストの制約」から移した手元の検証とtestの規則の経緯

goal 94のtask 1457が、AGENTS.mdの「変更後に必ず通す」「テストの制約」とplan reviewのverifyの選び方を開発文書（[手元の検証](../development/local-checks.md)・[testの制約](../development/testing.md)・[migration](../development/migrations.md)・[taskの登録](../development/task-registration.md)）へ移したとき、既存のADR・plansに無かった経緯をここに残す（[ADR-t1453-2](../adr/2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)決定1・4）。今の規則は開発文書が持ち、ここは書き換えない記録。見出しの括弧は[棚卸しの表](agents-slim-inventory.md)の項目のID。

## workerの手元のtestを関係する範囲に絞った（A-083）

2026-09-26に人がplannerと決めた（task 528）。理由: workerの全体の`cargo test`はintegrateのllvm-covと同じtestを全部流すので重複で、testファイルを分けてtest binaryが増えたぶんbuildとlinkが重く（その後ADR-0078でintegration testを1つのbinaryにまとめた）、hostのloadとworkerのwork時間を押し上げていた。変更前の基準値は2026-09-26の`dagq stats`の直近46 runで、work中央値1439秒、startup中央値1172秒、land_phases.verify中央値276秒。失敗の発見がintegrateに移るコストは、task 514の検証の重複の回数と、integrateの検証の失敗率（resume）で変更の前後を見て判断する。

workerのpromptはverification_commandsをintegrateが流すものとして見せ、手元の検証をrepositoryの文書に委ねる（task 510。今の仕組みは[prompt](../design/supervisor-lifecycle/prompt.md#workerのprompt)の「workerのprompt」のverification commandsの項）。

## 全体を比べる既存のtestも流す（A-064）

migration・`doctor` / `status`の出力・pluginの文書を変えたrunが、変えた機能と関係すると気づきにくいtest（`queue_migration::`・`cli_tasks::`・`--test plugin`など）を手元で流さず、`integrate`の関門で初めて落ちて数えられるresumeを使ってきたため、選び方の目安に足した（goal 67、finding 18）。

## stressを入れて、軽い見張りに縮めた（A-084）

2026-09-27に人がplannerと、足した・変えたtestをworkerの手元でstressにかけると決めた（task 767。不安定なtestを着地前に見つける案A。当時は20周、長いtestは`--stress-duration 5m`）。理由: 新しく足したtestが着地の後に別のtaskの`integrate`の検証で不安定に落ち、そのtaskのrunがresumeされて1件あたり7〜16分遅れた（commit 4f791e7が足した`runtime_abandon`のtestが例）。`integrate`の検証は1本ずつ直列に流れるので、そこで繰り返すと全部の着地が延びる。

2026-09-28に人がplannerと、軽い見張り（5周、長いtestは`--stress-duration 60s`）に縮めて重い繰り返しをCIの定時実行に移すと決めた（goal 62、[ADR-t920-1](../adr/2026-09-28-t920-1-light-worker-stress-and-heavy-repetition-in-scheduled-ci.md)。その「## Context」と決定が持つ理由の残り）。縮めた理由の値: task 767の後、runtimeのtaskのworkerの手元のtestの時間（statsの`work_breakdown.secs.test`）の中央値が150秒から442秒に伸び、stress自体がhostのloadを押し上げてloadによるclaimの保留（2026-09-28午前は時間の47%）を増やした。一方[ADR-t768-1](../adr/2026-09-27-t768-1-rerun-failed-tests-once-and-land-again-on-flaky-only.md)で、着地の検証で落ちたtestが全てFLAKYならresumeせずに着地を1回やり直すので、不安定なtestが着地したときの代償は検証1周分に小さくなった。定時実行で落ちたtestはGitHubのissue（同じtestは既存のissueに追記）で人に知らせ、plannerがtaskにする。効果の見方と周回を戻す条件はADR-t920-1決定3。

## `<module>::`で名指しし、再現の例外を検証の失敗に限った（A-086）

workerの手元のtestを`<module>::`で1つずつ名指しし、手元の`cargo llvm-cov`と全体のtestを`integrate`の検証が落ちたresumeでの再現だけに限り、e2eをrunの最後に1回（直すあいだは名前で絞って1本ずつ）にすると書き直したのは2026-09-30のtask 1030（task 932のfollow-up）。理由: 2026-09-27 04:00〜06:30のloadの山の間、workerが「変更に関係するtestだけ」の規則を外れて、`--test it -- runtime_ lifecycle_ ...`（`it`の大半）、`--test it --test plugin`（全体）、rebaseの衝突によるresumeでの`cargo llvm-cov nextest`（task 418のrun 3888fd0e）、e2eの繰り返し（task 314のworkerは7回）を手元で流しており、その重なりをloadの山の主因と見た（[load-spike-2026-09-27](load-spike-2026-09-27.md)の3章・4.1節・4.2節と5章の1・4）。runtimeのworkerとresumeのpromptはtask 510で手元の検証をrepositoryの文書に委ねているので、runtimeは変えずに、接頭辞の例（`tests/it/runtime_*.rs`など）が接頭辞のfilterを促し、再現の例外がresume全般に読める書き方を直した。並列度を分ける案と重いtestを待たせる案の続きは[load-spike-2026-09-27](load-spike-2026-09-27.md)の「6.4 見立てと次の判断」（task 1034）。e2eはその後2026-10-01の人の決定（[ADR-t1233-2](../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)、task 1239・1240）でworkerの手元から外れた。

## testの制約の出どころ（A-119・A-138・A-135・A-136・A-113）

- testファイルの3,000行の上限はgoal 47で決めた。
- testの待ちに上限を付け、`cargo test | tail`が戻らなくならないようにしたのはtask 324。
- e2eの印で通した回数とflakyの回数を関門のtestごとに数えるのはtask 1166、着地の前のe2eの印をlanding branchの着地したcommitのtreeから読み、workerの変更やmain checkoutの未commitの変更を効かせないのはtask 1198。
- taskのkindと`--kind`はtask 984で消した（[ADR-t980-1](../adr/2026-09-29-t980-1-classify-runs-by-declared-change-and-diff-derived-area.md)決定1）。
