---
id: design-supervisor-lifecycle-session-send
type: design
title: "sessionへの送信と確認"
status: current
created: 2026-09-26
updated: 2026-10-05 # task 1440: run screenをturnのlogのCLIへの案内とともに拒む
last_verified: 2026-10-05 # task 1440
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-authorization
  - adr-t1228-1
  - adr-t1533-1
---

# Session send

worker の session への打ち込み・入力欄の確認・Enter の送り直し・`StartCheck`・作業の兆候の確認・入力の消失の再送・`send_unconfirmed` の復旧 job は task 1437 で撤去した（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。過去の `submit_*` と `send_unconfirmed` の event は履歴として読める。

1. worker への依頼と答えは `deliver::submit` が `headless::request_turn` を通して run dir の `turns/` に書く。終了も終了依頼ファイルへ書く。
2. wrapper が次の turn として依頼を取り、turn の出力と結果を記録する。turn の依頼・終わり・receipt を使って監視する。
3. planner の対話と inbox の促しは引き続き `submit_input` を使う（task 1441・1442 まで）。text を打ち、入力欄に残る間は `submit_check_interval` ごとに Enter だけを最大3回送り直す。ダイアログが現れれば送らず `Dialog`、残れば `Stuck`、離れれば `Submitted` とする。
4. 対話の planner の終了に使う `Input::Exit` の送信と、送信失敗・時間切れの判定は残る。worker はこの打鍵の処理を使わない。
5. planner の最後の入力の印は `supervisor-input.json` へ書く。planner の screen idle と inbox の readiness は引き続き画面から判定する。
6. planner と inbox の上の処理の撤去は後続 task が担当する。


## 人とinboxの画面の読み取りと送信

[ADR-t1228-1](../../adr/2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)の決定4・5・7・8、task 1230。人とinboxは`cmux read-screen` / `send` / `send-key`を打たず、次のCLIでrunのturnの場所を知り、plannerのsessionの画面を読み、送る（実装は`src/application/screen.rs`、CLIは`src/main.rs`の`run`と`planner`の群。隠しサブコマンド`session`とは別の名前）。宛先はrunのID（数字ならtaskのIDで、そのtaskの最新のrun）かplannerのIDで、workspaceのUUIDは引数に取らず、plannerの画面を読む・送るときだけqueueの記録（plannerの`workspace_id`）から引く。

| コマンド | すること |
| --- | --- |
| `run screen RUN [--lines N]` | どのrunにも、理由と代わりを示して拒む（非0で終わり、JSONのerror）。理由はrunのsessionが画面なしのbackgroundで動くこと（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）と対話のworkerを廃止したこと（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)、task 1437）で、代わりはturnのlogを読む`dagq run log RUN`（`--follow`で追う。task 1406）とrun dirの`turns/`の場所。先にrunを引き（知らないrun・taskはqueueのerror）、画面も出力も読まず、`screen_read`を残さない |
| `planner screen ID [--lines N]` | 画面（cmuxの`read-screen`）の末尾のN行（末尾の空行を除く）を返す。既定40行、最大200行で、超える指定は200に切る（`lines_requested`・`lines_limit`・`lines`・`truncated`・`screen`） |
| `run send RUN --key K [--key K ...]` | どのrunにも、対話の入力が無い理由と代わりを示して拒む。答えは`answer`で記録し、supervisorが次のturnとして届ける |
| `planner send ID --key K ...` | 決めたキーの集合だけを順に送る（cmuxの`send-key`）。集合は`enter`・`escape`・`up`・`down`・`1`〜`9`（ダイアログの番号）・`exit`。1回に10個まで。`exit`は単独でだけ送れ、`/exit`をsupervisorと同じ`submit_input`（`Input::Exit`。打ち直さない）で打つ |
| `run send RUN --answer ASK` | キーと同じ理由と代わりを示してどのrunにも拒む。画面もaskの答えも打ち込まず、`ask_delivered`は残さない |
| `planner send ID --answer ASK` | 答えられた`planner_question`のうち、答えがそのplannerに届くもの（`planner_answer_route`が同じplanner）の答えを同じ形で打つ。supervisorと同じく先に`claim_planner_answer`（`planner_answer_claimed`）で打つ権利を取り（取れなければ拒む）、打った後に`ask_delivered`を記録してaskを閉じる。ほかのaskは拒む |

- 自由な文・集合の外のキー・`--key`と`--answer`の両方・どちらも無い送信は、cmuxを呼ぶ前に拒む。workerのaskへの答えは`answer`に記録し、supervisorが次のturnとして届ける
- runの画面は読まない。`run screen`は上の拒否を、閉じたrun・過去にworkspaceで開いたrun・過去の対話のrunにも返す。`run send`はどのrunにも拒む。どちらもqueueだけを読み、cmuxもagentのadapterも作らない（`--cmux`・`--lines`・`--key`・`--answer`は受け付けて使わないので、cmuxの無いhostでも同じ応答になる）。閉じたplanner（`closed_at`）・workspaceの無いplannerは従来どおり拒む
- backgroundで動く非対話のsession（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)）のturnの要約は、画面の代わりに`run log RUN [--lines N] [--follow]` / `planner log ID [--lines N] [--follow]`でそのlogを読む（終わったrun・閉じたplannerも。capabilityは`screen.read`で、何も記録しないのでqueueを読み取りで開く。[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)の「logを読むCLI」）
- 記録: plannerの画面を読むと`screen_read`、送ると`screen_input_sent`を残す。plannerはqueueのevent（`planner_id`）で、eventのactor（呼び出し元のroleとid）を持つ。payloadは`target: planner`・`workspace_id`と、読み取りは行数、送信は`input`（`keys` / `answer`）・`keys`か`ask_id`・`outcome`（キーは`sent`、打った文と`/exit`は`submitted` / `dialog` / `stuck` / `unsent`）・`retries`。画面の中身はeventに載せない。どちらもattentionにはしない
- 判定: capabilityは`screen.read`・`screen.send`で、userとinboxだけが持つ（[Authorization](../authorization.md)）。`Operation`の入口で判定し、拒めば`authorization_denied`を残す。plannerの読み取りと送信はeventを残すので、どちらも状態を変えるコマンドとしてqueueを書き込みで開く（本番queueでは固定バイナリで打つ）
- 送信の判定（`AgentSignals`）はClaude Codeの画面のもの（`ClaudeCode`）を使う。画面を持つsessionはClaude Codeだけのため
- 非対話のruntimeのplanner（`route: headless`）には、supervisorはこの文書の送信と確認を使わない。reviseの指摘・`planner_question`のanswer・Claudeが使えなかったturnの続き（`provider retry`）・終了は、`Supervisor::send_to_planner`が次のturnの依頼と終了の依頼としてplannerのディレクトリの`turns/`に置く（[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)、ADR-t1394-2決定2）。非対話のplannerには`planner screen`が画面を読まず`screen: null`とplannerのディレクトリの`turns/`の場所を返し（`screen_read`は残さない）、`planner send`は`--key`も`--answer`も拒む（[ADR-t1533-1](../../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)）。人の言葉を足す続きの依頼は、cmuxでの送信でなく`planner request`が次のturnの依頼として置き、対話のplannerには拒む（[続きの依頼と非対話のplannerのCLI](plan-planners.md#続きの依頼と非対話のplannerのcli)）
