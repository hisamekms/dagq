---
id: plan-operation-rules-history
type: plan
title: AGENTS.mdの「作業中」「起動と停止」「着地と人の判断」から移した運用の規則の経緯
status: completed
created: 2026-10-03
updated: 2026-10-03
owners:
  - hisamekms
tags:
  - operations
  - documentation
related:
  - adr-t1453-2
  - development-operations
  - plan-agents-slim-inventory
---

# AGENTS.mdの「作業中」「起動と停止」「着地と人の判断」から移した運用の規則の経緯

goal 94のtask 1458が、AGENTS.mdの3節の規則を[運用の開発文書](../development/operations.md)などの正本へ移したとき、既存のADR・plansに無かった経緯をここに残す（[ADR-t1453-2](../adr/2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)決定1・4）。task 1503が`dagq.toml`のコメントから除いた経緯のうち、ほかのADR・plansに無かったものもここに足した。今の規則は開発文書、今の値は`dagq.toml`が持ち、ここは書き換えない記録。項目のIDは[棚卸しの表](agents-slim-inventory.md)のもの。

## 使い捨てのqueueをworkerに使わせない（A-014）

workerが使い捨てのqueueで`init`・`add`・`up`を使えないのは、authorizationのpolicy（[ADR-t728-1](../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)）がworkerに`queue.admin`を与えないため（task 983で表に出た）。2026-09-29にask 195で、人はpolicyを変えないと決めた。workerは実バイナリの振る舞いをtestで確かめ、実queueでの手の確認は`follow_ups`（`ops`）で人かinboxに任せる。

## `[run.env]`の`CARGO_BUILD_JOBS`を4にした（A-035）

2026-09-26に人がplannerと決めた（task 427）。hostは8コア / 16GBで、並列4の運用でload averageが最大151〜204に達し、cmuxのcaptureのtimeoutが400件を超え、runのstartupの中央値が約1100秒になった（goal 36のnote 8718の基準値）。supervisorの並列数を3に下げ、worker 3本と`integrate` 1本が同時にcargoを回しても合計16並列（コア数の2倍）程度に収まるように4にした。`dagq.toml`の`[run.env]`のコメントは今の値の理由の要点とこの節への参照だけを持つ（task 1503でコメントから経緯を除いた）。testの並列度の4→8→6の経緯と測定は[nextest-test-threads](nextest-test-threads.md)の7章・8章が持つ。

## `[supervisor]`の`runtime_planners`を2にした（A-187）

task 1503が`dagq.toml`の`[supervisor]`のコメントから除いた経緯。2026-09-28に、`planner_question`（ask 176）の答えを待つplannerがruntimeのplannerの1枠を約5.5時間ふさぎ、follow_upなどのdraft約20件が決まらず、proposal 283のreviseも届かずに`planner_unresponsive`になった。答えを待つplannerが枠を空ける根本の対応は人が別途行うことにし、当面の緩和として上限を2にした（goal 63、task 941が`runtime_planners`を足し、固定バイナリがtask 941を含んでからtask 942が`dagq.toml`に足した）。同じ詰まりの時間の分け方は[follow-upの種類](follow-up-kinds.md)にもある。

## `[supervisor]`の`parallel`を`dagq.toml`に移した

task 1503が`dagq.toml`の`[supervisor]`のコメントから除いた経緯。以前はAGENTS.mdの`up`のコマンドが`--parallel 3`で並列数を渡していた。固定バイナリがtask 698を含んでから、task 699が`[supervisor]`に`parallel = 3`を足し、`up`のコマンドから`--parallel 3`を外した。3は`[run.env]`の`CARGO_BUILD_JOBS`を決めたとき（task 427、上の節）の並列数で、`[run.env]`の並列度はこれを前提にする。

## 本番のsupervisorを`--auto-update`に切り替えた1回だけの手順（A-194）

2026-09-27に本番を`up --auto-update`に切り替えたとき（[ADR-0073](../adr/0073-kind-additions-are-compatible.md)決定17）、AGENTS.mdは次の1回だけの手順を持っていた。切り替えは済み、今の`up`のコマンドは[運用の開発文書](../development/operations.md)の「`up`のコマンド」が持つ。

1. `dagq status`でsupervisorの`binary_version`と`auto_update`を見る。
2. `dagq install --claude ~/.local/bin/claude --codex ~/.local/bin/codex --plugin-dir <この repository>/plugins/claude-dagq`で固定バイナリをmainの最新にする。supervisorは引き継ぎで入れ替わり、runは止まらない。非互換のmigrationが未適用で止まったら、開いたaskを人に見せ、了承を得て同じコマンドに`--allow-breaking`を付けて打ち直す。結果の`not_handed_off`に載ったsupervisor（引き継ぎより前のバイナリ）は3の`up`がdrainして入れ替える。
3. `--auto-update`付きの`up`を打つ。build識別子が同じならsupervisorは`reused`のままで、登録に自動更新が書かれる。
4. `dagq status`の`auto_update.enabled`と`supervisors[].auto_update`が`true`であることを確かめる。以後はruntimeを変える着地ごとに自動で入れ替わるので、inboxが着地ごとに`install`を打つ必要はない。
