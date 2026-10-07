---
id: design-supervisor-lifecycle-session-send
type: design
title: "sessionへの送信と確認"
status: current
created: 2026-09-26
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
3. `submit_input` を使うのは inbox の促し（task 1442）だけになった。人の planner には打ち込まない（人の planner の行は cmux を呼ばずに閉じ、revise と answer は新しい runtime の planner が受ける）。runtime の planner は非対話だけで、打ち込まない（task 1441）。text を打ち、入力欄に残る間は `submit_check_interval` ごとに Enter だけを最大3回送り直す。ダイアログが現れれば送らず `Dialog`、残れば `Stuck`、離れれば `Submitted` とする。
4. `Input::Exit` の送信と、送信失敗・時間切れの判定は `submit_input` に残るが、今これを使う処理は無い（どの planner にも打たない。worker と runtime の planner はこの打鍵の処理を使わない）。
5. plannerの最後の入力の印は `supervisor-input.json` へ書く（非対話の依頼の前。どの planner にも打たない）。inbox の readiness は引き続き画面から判定する。人の planner の画面からは idle を判定しない。
6. inbox の上の処理の撤去は後続 task（1442）が担当する。


## 人とinboxの画面の読み取りと送信

[ADR-t1228-1](../../adr/2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)の決定4・5・7・8、task 1230。人とinboxは`cmux read-screen` / `send` / `send-key`を打たず、次のCLIでrunとplannerのturnの場所を知る（実装は`src/application/screen.rs`、CLIは`src/main.rs`の`run`と`planner`の群。隠しサブコマンド`session`とは別の名前）。宛先はrunのID（数字ならtaskのIDで、そのtaskの最新のrun）かplannerのIDで、workspaceのUUIDは引数に取らない。どのコマンドも画面を読まず、打ち込まない（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)、runはtask 1437、plannerはtask 1441）。

| コマンド | すること |
| --- | --- |
| `run screen RUN [--lines N]` | どのrunにも、理由と代わりを示して拒む（非0で終わり、JSONのerror）。理由はrunのsessionが画面なしのbackgroundで動くこと（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）と対話のworkerを廃止したこと（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)、task 1437）で、代わりはturnのlogを読む`dagq run log RUN`（`--follow`で追う。task 1406）とrun dirの`turns/`の場所。先にrunを引き（知らないrun・taskはqueueのerror）、画面も出力も読まず、`screen_read`を残さない |
| `planner screen ID [--lines N]` | どのplannerにも画面を読まず、cmuxも呼ばずに、task 1533の形（`planner_id`・`route`・`screen: null`・`reason`・plannerのディレクトリの`turns/`の場所の`turns`）を返す。`reason`は非対話のplannerなら画面が無いこと、workspaceのplanner（人のplannerか、古いバイナリが開いた対話の行）ならその画面はもう読まず、自分の端末でそのworkspaceを読むこと。`screen_read`を残さない |
| `run send RUN --key K [--key K ...]` | どのrunにも、対話の入力が無い理由と代わりを示して拒む。答えは`answer`で記録し、supervisorが次のturnとして届ける |
| `planner send ID --key K ...` | どのplannerにも、cmuxを呼ぶ前に理由と代わりを示して拒む。非対話のplannerは画面が無くキーも答えも取らず、続きの依頼は`planner request`で渡す。workspaceのplannerにももう打ち込まず、questionには`answer`で答える |
| `run send RUN --answer ASK` | キーと同じ理由と代わりを示してどのrunにも拒む。画面もaskの答えも打ち込まず、`ask_delivered`は残さない |
| `planner send ID --answer ASK` | キーと同じ理由と代わりを示してどのplannerにも拒む。`planner_question`の答えは`answer`で記録し、supervisorが届ける（runtimeのplannerには次のturnとして）。claimも打鍵もせず、`ask_delivered`は残さない |

- workerとplannerのaskへの答えは`answer`に記録し、supervisorが届ける（workerと非対話のplannerには次のturnとして）
- runの画面は読まない。`run screen`は上の拒否を、閉じたrun・過去にworkspaceで開いたrun・過去の対話のrunにも返す。`run send`はどのrunにも拒む。どちらもqueueだけを読み、cmuxもagentのadapterも作らない（`--cmux`・`--lines`・`--key`・`--answer`は受け付けて使わないので、cmuxの無いhostでも同じ応答になる）。`planner screen` / `planner send`も同じく、plannerの記録だけを読み、cmuxを作らず、引数を受け付けて使わない（知らないplannerはqueueのerror）
- backgroundで動く非対話のsession（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)）のturnの要約は、画面の代わりに`run log RUN [--lines N] [--follow]` / `planner log ID [--lines N] [--follow]`でそのlogを読む（終わったrun・閉じたplannerも。capabilityは`screen.read`で、何も記録しないのでqueueを読み取りで開く。[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)の「logを読むCLI」）
- 記録: どのコマンドも`screen_read` / `screen_input_sent`を残さない（task 1441まではplannerの画面の読み取りと送信が残した。過去のeventは履歴として読める）
- 判定: capabilityは`screen.read`・`screen.send`で、userとinboxだけが持つ（[Authorization](../authorization.md)）。`Operation`の入口で判定し、拒めば`authorization_denied`を残す。`planner screen` / `planner send`はもうeventを残さないが、queueは今も書き込みで開く（本番queueでは固定バイナリで打つ）
- supervisorがinboxに打つときの送信の判定（`AgentSignals`）はClaude Codeの画面のもの（`ClaudeCode`）を使う。画面を持つsessionはClaude Codeだけのため
- runtimeのplannerはどれも非対話（`route: headless`）で、supervisorはこの文書の送信と確認を使わない（古いバイナリがworkspaceで開いたruntimeのplannerの行にも打ち込まない）。reviseの指摘・`planner_question`のanswer・Claudeが使えなかったturnの続き（`provider retry`）・終了は、`Supervisor::send_to_planner`が次のturnの依頼と終了の依頼としてplannerのディレクトリの`turns/`に置く（[runtimeのplannerの経路](plan-planners.md#runtimeのplannerの経路)、ADR-t1394-2決定2）。`planner screen`の`screen: null`と`turns`の形と、非対話のplannerへの`planner send`の拒否は[ADR-t1533-1](../../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)のもの。人の言葉を足す続きの依頼は、cmuxでの送信でなく`planner request`が次のturnの依頼として置き、対話のplannerには拒む（[続きの依頼と非対話のplannerのCLI](plan-planners.md#続きの依頼と非対話のplannerのcli)）
