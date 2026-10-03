---
id: adr-t1433-2
type: adr
title: 対話の経路を廃止し、Claudeのworkerとruntimeのplannerを非対話だけで動かす。interactiveで登録済みのtaskは非対話でclaimして記録し、add / editの--interactiveは理由付きで拒み、人はturnのlogのCLIとask / answerでworkerを見て伝え、過去の対話のrunは読めるまま残す（ADR-t1340-1とADR-t803-1を置き換え、ADR-t1404-1決定6・ADR-t1394-2決定1と対話にだけ効く決定をamends）
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
supersedes:
  - adr-t1340-1
  - adr-t803-1
amends:
  - adr-t813-1 decision 7
  - adr-t813-2 decision 1
  - adr-t813-2 decision 5
  - adr-t1394-2 decision 1
  - adr-t1404-1 decision 6
  - adr-0027 decision 1
  - adr-0027 decision 2
  - adr-0027 decision 3
  - adr-0027 decision 4
  - adr-0071 decision 1
  - adr-0071 decision 2
  - adr-0071 decision 6
  - adr-0071 decision 10
  - adr-0071 decision 17
  - adr-0047 decision 25
  - adr-0047 decision 29
  - adr-0047 decision 30
  - adr-0047 decision 31
  - adr-0047 decision 34
  - adr-0047 decision 36
  - adr-0047 decision 37
  - adr-0047 decision 39
  - adr-0047 decision 42
  - adr-t1228-1 decision 2
  - adr-t1228-1 decision 4
  - adr-t1228-1 decision 5
  - adr-t609-1 decision 1
  - adr-t1394-1 decision 1
  - adr-t1394-1 decision 7
  - adr-t1394-1 decision 9
owners:
  - hisamekms
tags:
  - runtime
  - worker
  - planner
  - provider
related:
  - adr-t1340-1
  - adr-t803-1
  - adr-t813-1
  - adr-t813-2
  - adr-t1394-2
  - adr-t1404-1
  - adr-t1228-1
  - adr-t609-1
  - adr-0027
  - adr-0047
  - adr-0071
  - adr-0073
  - adr-t1091-1
  - adr-t1433-1
  - adr-t1433-3
  - design-provider-lifecycle
  - design-supervisor-lifecycle-headless-worker
---

# ADR-t1433-2: 対話の経路を廃止し、Claudeのworkerとruntimeのplannerを非対話だけで動かす

## Context

[ADR-t1340-1](2026-10-02-t1340-1-claude-worker-defaults-to-headless.md)はClaudeのworkerの既定を非対話にし、対話の経路はtaskごとに`--interactive`を選んだときだけ使うとした。[ADR-t1394-2](2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)決定1はruntimeのplannerの経路を`dagq.toml`で対話と非対話から選べるようにし、[ADR-t1404-1](2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)決定6は画面で見たい人に`--interactive`を選ぶ道を残した。

2026-10-03に人は「cmuxはinboxだけが使う」方針を採った（goal 92、[ADR-t1433-1](2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)）。対話の経路はcmuxの画面と打ち込みでしか動かないので、残せばworkerとplannerがcmuxを呼び続ける。goal 92の計測では、対話に固有のtest（画面・ダイアログ・`/exit`・Enterの送り直し・`answer_prompt`・`stuck_exit`）が約230件・関門のtestの時間の約28%を占め、fixtureの既定が対話のために約400件のtestが偽のcmuxを通る。goal 87のtask 1403とgoal 89のtask 1409の1週間の評価の「対話に戻す」の選択肢は、人の決定で先に閉じる。

ADR-t1340-1は決定が1つなので丸ごと置き換える。画面からidleを推定する[ADR-t803-1](2026-09-27-t803-1-infer-idle-from-the-screen-when-the-idle-marker-is-missing-or-stale.md)は6つの決定のすべてが対話のsession（plannerとworkerの画面）だけに効き、inboxの画面を読む後ろ盾も[ADR-t1433-5](2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)が無くすので、丸ごと置き換える。

## Decision

1. **Claudeのworkerの経路は非対話だけにする（ADR-t1340-1を置き換え、ADR-t813-1決定7、ADR-t813-2決定1をamends）。** ADR-t1340-1の決定のうち、次を引き継ぐ。
   - 経路を指定しないClaudeのtaskは非対話の経路で動く。Codexのrunは非対話の経路だけを使う。
   - 既定と明示した経路を保存の上で区別する。既定はruntimeの既定として持ち、`dagq.toml`の欄にしない。
   - まだclaimされていない古い既定（対話）を保存したtaskを新しい既定に従わせる互換のmigrationは、着地済みのものをそのまま有効とする。経路を明示したtask、claim済みのtask、過去のrunの記録は変えない。

   「対話の経路はtaskごとに`--interactive`を選んだときだけ使う」は廃止する。対話の経路はどのtaskにも選べない。
2. **`interactive`で登録済みのtaskの扱い。** 人が`--interactive`を明示して登録した、まだclaimされていないtaskは、保存を書き換えず、claimのときに非対話の経路で動かし、頼まれた経路と実際の経路が違うことをrunの記録に残す。`add` / `edit`の`--interactive`は受け付けず、対話の経路が廃止されたことと代わり（決定4）を理由に書いて拒む。保存を書き換えるmigrationを足さないのは、走っている古い固定バイナリが新しいqueueを読めるまま入れ替えるため（goal 92のconstraints、[ADR-0073](0073-kind-additions-are-compatible.md)）。
3. **runtimeのplannerも非対話だけにする（ADR-t1394-2決定1をamends）。** `[roles.runtime_planner]`の経路の欄で対話を選ぶことはやめ、plannerは決定2〜7の非対話の形だけで動く。欄の扱い（受け付けて無視し、後でdagq.tomlから消す）はADR-t1433-3の切り替えの欄と同じにする。
4. **人がworkerを見たい・伝えたいときの代わり（ADR-t1404-1決定6をamends）。** 決定6のうち、wrapperの`[dagq]`の要約をrun dirのlogに書き、人がdagqのCLIでそのlogを読み、追う部分（task 1406のturnのlogのCLI）は引き継ぐ。runtimeが画面を付ける手段を持たないことも変えない。「その taskに対話の経路（`--interactive`）を選ぶ」選択肢は廃止する。人は見るのにturnのlogのCLIと`dagq timeline`を使い、伝えるのにworkerが開いた`worker_question`へのanswer、reviewの`send_back`、`stalled`などのaskへのanswerを使う（[ADR-t813-1](2026-09-28-t813-1-headless-worker-path.md)決定4のまま）。
5. **対話のsessionにだけ効く決定は対象が無くなる（amends）。** 次の決定のうち、画面・idleの印（Stop hook）・`/exit`・ダイアログ・入力欄の確認とEnterの送り直し・terminalへの`cmux send`に当たる部分は、対象のsessionが無いので効かない。非対話の読み替え（ADR-t813-1決定5・6・9）だけが残る。
   - [ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)決定1〜4: reviewの後までsessionを残すこと、reviseの回数とverdict、merge-treeの事前判定は変えず、`/exit`と`cmux send`はturnの依頼と最後のturnの終わりに読む。
   - [ADR-0071](0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)決定1・2・6・10・17: `answer_prompt` / `stalled`（画面の）/ `stuck_exit`の待ち、`dialog_cleared` / `session_moved`、`Resume`の段のダイアログの検知は起きない。待ちは`worker_question`と復旧jobのescalateのaskだけになる。
   - [ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定25（`/exit`の再試行）・29（prompt待ちと既知のダイアログ）・31（Enterの送り直し）・36（Bの型の`/exit`の経路）は対象が無い。決定30（receiptの無いidle）・34（`running_alerts`）・37（ケースの表）・39（alertの一覧）は、非対話の行（ADR-t813-1決定9）だけが残り、`stuck_exit`・`prompt_waiting`・`long_background`・画面の`stalled`の行とalertは無くなる。
   - ADR-0047決定42（認証とコストのask）: 認証切れと利用上限の検知は「workerの画面」を除いた非対話のturnの出力・復旧jobの入力・headless jobの出力から行い、`done`の後の「続けて」は止まったsessionへの打ち込みでなく次のturnの依頼として送る（ADR-t813-1決定2と同じ）。
   - [ADR-t1394-1](2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)決定1・7・9（廃止の時点で開いていた人のplanner）: その人のplannerの対話のworkspaceを動かし続け、reviseを配送し、`planner_question`をそのworkspaceで人に聞き、agentの終了の後にruntimeがworkspaceを閉じる移行の例外は、画面と打ち込みとcmuxの呼び出しを要るので残さない。新しいバイナリは、まだ開いている人のplannerの行を、runtimeがplannerを閉じたときと同じ記録（[ADR-t1300-1](2026-10-02-t1300-1-runtime-closes-exited-person-planners-after-a-grace.md)決定2のevent）を残して閉じた行として扱い、そのworkspaceにはsendもcloseもしない。持ち主の居なくなったproposalへのreviseとanswerは、持ち主のplannerが閉じているときの今の経路（runtimeが新しい非対話のplannerを立てる。ADR-0047決定12、ADR-t1394-1決定7）で届ける。残ったworkspaceは人が自分のterminalで閉じる（ADR-t1433-3決定3の残ったworkspaceと同じ扱い）。人のplannerはADR-t1394-1で新しく開けないので、これは本番に残る数個の行への一度きりの扱いである。
   - [ADR-t609-1](2026-09-27-t609-1-failed-live-recovery-job-opens-the-alert-ask.md)決定1: 対象のalertは非対話の生きているrunのalert（出力の途絶え・turnの時間の上限など）だけになり、規則（jobの失敗でそのalertのaskを開く）は変えない。
   - [ADR-t1228-1](2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)決定2（plannerへの依頼）: 宛先の「生きている対話のplanner」とterminalへの送信は対象が無く、ADR-t1394-2決定7の次のturnの依頼として置く形だけが残る。決定4・5（runとplannerの画面を読む・sessionに送る）: 画面を持つsessionが無いので、`run screen` / `run send` / `planner screen` / `planner send`は対象が無く、画面の代わりにturnのlogのCLIを指す。答えは`answer`でsupervisorが次のturnとして届ける。
   - [ADR-t813-2](2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)決定5（対話のClaudeのrunが止まったら非対話のCodexへ）: 対話のrunが無いので、非対話どうしの切り替え（決定4）だけが残る。
6. **過去の対話のrunの記録は読めるまま残す。** `kpi --by route`・`stats`・`show`・`timeline`は、過去の対話のrunを経路`interactive`のまま読み、集計から外さない。経路の記録（ADR-t813-2決定7）は変えない。

ADR-t803-1の決定のうち引き継ぐものは無い。非対話のworkerとplannerのidleはturnの終わりで決める（ADR-t813-1決定1、ADR-t1394-2決定3）。

実装はgoal 92の後続のtaskが行う。拒む文面・記録のevent・CLIの綴りは[provider-lifecycle](../design/provider-lifecycle.md)と[非対話のworker](../design/supervisor-lifecycle/headless-worker.md)に書く。

## Alternatives

- **対話を残し、cmuxのfakeを使うtestだけを減らす**: 対話の画面・ダイアログ・`/exit`の処理とそのtest（関門の約28%）が残り、workerとplannerがcmuxを呼び続ける。対話を選ぶtaskは本番でほぼ無く、残す費用に見合わない。
- **評価（task 1403・1409）の後に決める**: 評価は非対話が退行していないかの確認として残す。人は戻す選択肢を先に閉じると決めた。
- **`interactive`で登録済みのtaskをmigrationで書き換える**: 走っている古いバイナリとの互換を崩す段を作る。claimのときに読み替えれば足りる。
- **`--interactive`を黙って受け付けて無視する**: 人が対話を選んだつもりで非対話になり、理由が残らない。拒んで代わりを示す。

## Consequences

- workerとruntimeのplannerはcmuxを呼ばず、画面・idleの印・`/exit`・ダイアログ・Enterの送り直しの処理と、それだけを確かめるtestを消せる。共通の判断（状態の遷移・回数・上限・verdict）を確かめるtestは非対話の経路かunit testに移す（task 1410の方針）。
- 人はworkerの画面を見られず、logのCLIで追う。workerに打ち込む道は無く、伝えることはaskとanswerに残る。
- `interactive`を明示して登録したtaskは、頼んだ経路と違う経路で走ったことが記録で分かる。
- task 1403とtask 1409の1週間の判定は戻す判断ではなく、退行の確認として読む。
