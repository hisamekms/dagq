---
id: design-supervisor-lifecycle-idle-without-receipt
type: design
title: "receiptの無いidleの検知"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0043
  - adr-t803-1
---

# receiptの無いidleの検知

worker はすべて非対話の turn で動く（task 1437、[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。画面と Stop hook の経過時間で検知する対話の worker の処理は廃止した。

- **workerのrun**（[ADR-t813-1](../../adr/2026-09-28-t813-1-headless-worker-path.md)の決定9、task 815）: turnの終わりが決まるので閾値を待たずに促し、促しは2回まで（`HEADLESS_NUDGES`）、使い切ったら理由`turn_without_receipt`、turnの`permission_denials`が3件以上なら促さずに理由`permission_denied`の復旧jobにする。画面は読まない。詳細は[非対話のworker](headless-worker.md#turnの後の扱い)。`stalled`のaskの選択肢は`intervene`の代わりに`stop`を持ち（`wait` / `stop` / `propose`とjobの足したもの。questionは答える前にturnの記録を読むことを書く。task 1179）、`stop`と答えるとsupervisorがsessionに終了の依頼を1回だけ書いてaskを閉じ（`stall_resolved`の`answered_stop`）、runはreceiptの無いまま終わったsessionとして`failed`になり復旧job（alert `failed`）にかかる（task 1104。`stop`は次のturnに送らない）。それでも届いた`intervene`はheldにせず、askを閉じて`answered_intervene`を1回記録し、すぐに非対話のoptionsで新しいaskを開き直す（task 1179。次のturnに送らない。仕組みと引き継ぎは同じ文書）。`wait`・`intervene`・`stop`・`propose`以外の答えは次のturnの依頼にし、supervisorが依頼を書いた後に止まっても引き継いだsupervisorは同じ答えを2回送らない（task 863。仕組みは同じ文書）。

- **対象**: 最初の session の turn が receipt も未回答の worker_question も残さずに終わったとき。resume は解消依頼の turn の後に receipt を判定し、revise は差し戻し依頼の turn の後に書き直しを判定する。
- **答えずに閉じた質問**: `closed_undelivered` の質問は `closed_question_notice` を次の turn として一度届ける。`stall_nudged.closed_ask` に記録し、引き継ぎ後の二重配達を防ぐ。依頼を書くのに失敗した場合は記録せず、NOTICE_ATTEMPTS まで再試行する。runtime_headless_reopen の `a_question_closed_without_its_answer_is_told_as_the_next_turn` と domain::worker_question の unit test が確認する。
- **復旧と回答**: `stalled` の復旧 job は turn の記録を受け取る。高い確信の repair は前提を再確認して適用し、失敗・低い確信・上限超えは ask にする。`wait` は待ちを再開し、`stop` は終了を依頼する。ほかの指示は次の turn として届ける。過去の `intervene` の答えは保持状態にせず、非対話の選択肢で ask を開き直す。
- **閉じる・記録**: receipt、質問、次の turn、run の終了などで検知を終える。`stall_resolved` は促し・job・ask の各検知に一度書く。引き継ぎでは run_events の記録を使う。recover・abandon・triage・sweep が終える検知も同じ共通処理で記録する。
- **判断と境界**（[ADR-t1410-1](../../adr/2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)、[Architecture](../architecture.md)のC6）: 見張りの次の一手と引き継いだ見張りの組み立て直しは、`src/application/supervise/stall.rs`の副作用のない関数が決める。
  eventのpayloadは`domain::run::payload`の型で読み、欄の欠けと型の違う値は欄が無いものとして読む。
  時刻とidle markerの時刻、receipt・質問・holdの有無は値で受け、queue・file・sessionの読み書きは結果を実行する薄い処理だけが行う。
  判断はunit testが、askの開き方・復旧jobの起動・引き継ぎの配線は`tests/it`の代表のcaseが確かめる。

## 過去の記録

`idle_without_receipt`、`send_unconfirmed`、`stall_preempted`、`answered_intervene`、`prompt_waiting`、`answer_prompt` と画面由来の閾値の記録は読める。
stats は過去の検知と結末を引き続き集計する。
新しい検知には画面・打鍵・Enter の確認を使わず、どの session の画面からも idle を判定しない。
