---
id: design-supervisor-lifecycle-task-hold
type: design
title: taskのhold（保留）と解除（未実装）
status: draft
created: 2026-10-07
scope: runtime
related:
  - adr-t1879-1
  - adr-t1521-2
  - adr-t1850-1
  - adr-0049
  - adr-t1662-1
  - adr-t1662-2
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-task-replanning
  - design-supervisor-lifecycle-claim-hold
  - design-supervisor-lifecycle-queue-hold
  - design-supervisor-lifecycle-waiting
  - design-supervisor-lifecycle-triage
  - design-supervisor-lifecycle-needs-session
  - design-measurement
---

# taskのhold（保留）と解除（未実装）

**全節が予定の契約で、まだ実装していない。**
決定は[ADR-t1879-1](../../adr/2026-10-07-t1879-1-hold-a-task-at-the-turn-boundary-and-release-it-explicitly.md)が持ち、この文書は実装の正本になる。
今の挙動は関係する各文書の今の節のとおりで、この文書を足しただけではどのtaskも待たない。
実装したら、ここに書いたflag・eventの欄・既定値の意味は定義のそばのdoc commentへ移し、この文書には流れ・境界・入口だけを残す。

workerのrunは全て非対話（turnごとに別のprocess）で、この文書はその経路だけを扱う。
過去に記録された対話のrunの記録は、holdの判定でも他の記録と同じに読むだけで、対話の経路を足さない。

## 目的と境界

- 環境・認証・外のサービスの都合でtaskを待たせる間、supervisorがそのtaskを拾わないことを保証する。
- holdはtaskの印で、taskのstatusを変えない。
  readyのtaskはreadyのまま、`in_progress`のtaskは`in_progress`のままで、plan reviewを通った扱いを失わない。
- runを名指したholdは、そのrunのtaskのholdとして記録し、名指したrunのIDは記録の材料に残すだけにする。
- draft・cancel・`recover`・`run send`・`set-priority`・`edit`の意味と拒否は変えない。
  holdの印はこれらの操作で外れず、taskが終端（`completed`・`canceled`）になると効きを失う（解除とは数えない）。
- 走っている工程は止めない（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定4、非preemption）。
- queue全体・provider・areaの範囲のholdは作らない。
  queue全体の控えは[claimを控える](claim-hold.md)と[認証と利用上限のaskの待ち](queue-hold.md)が持ち、taskのholdはそれと独立に重なる。

## 印と確定

- 状態はqueue.dbの新しい表（互換のmigrationで足す）に、taskごとに高々1つの開いたholdとして持つ。
  持つのは理由・期限・かけたactor・かけた時刻・名指したrun・安全な待ちに入った時刻・解除の時刻と解除の選択。
- holdをかける操作は、表の行と`task_held`のeventを1つのtransaction（`BEGIN IMMEDIATE`）で書く（[ADR-t1662-2](../../adr/2026-10-04-t1662-2-measurement-stores-ssot-and-views.md)決定3）。
  同じtransactionでtaskとそのrunのstatus・走っている工程を読み、`settling`か`held`かを決める。
  同じtaskに開いたholdがあれば、理由を書き足さずに拒む（理由を変えたいときは解除してかけ直す）。
- 終端のtaskへのholdは拒む。
  着地の途中（`integrating`）のrunを持つtaskへのholdは受けるが、着地を止めない（下の「状態ごとの効果」）。
- holdを確かめる読み取りは1つの関数（予定の`domain::task_hold`）にまとめ、claim・resume・復旧job・回答の適用・着地の各入口がそれを呼ぶ。

## 状態ごとの効果

| taskとrunの状態 | holdの効果 | 安全な待ちに入る時 |
| --- | --- | --- |
| ready（runが無い） | claimの候補から外す | holdをかけた確定と同時 |
| `in_progress`で、runのturn・validation・review・e2eが走っている | 走っている工程を終わりまで行かせ、その後の工程を始めない | 走っていた工程が終わり、次の工程を始めずに待ちへ移した確定 |
| `in_progress`で、runが人の答えを待っている（[待ち](waiting.md)） | 答えが来ても次のturnに届けない。人の答えの待ちを終え（`--max-waiting`と`waiting.count`から外す）、holdの待ちに移す | wrapperの終了を確かめた確定 |
| `in_progress`で、runが着地の順番を待っている（`awaiting_integration`） | 着地を始めない | holdをかけた確定と同時 |
| `in_progress`で、runが着地の途中（`integrating`） | 着地を止めない | 着地が`needs_session`か`awaiting_integration`に戻ったとき（着地したらtaskが終わりholdは効きを失う） |
| `in_progress`で、runが止まっている（`failed`・`interrupted`） | 復旧jobを起動しない | 復旧jobが走っていなければ確定と同時、走っていればその終わり |
| `in_progress`で、runがresumeを待っている（`needs_session`） | resumeしない | holdをかけた確定と同時 |

- 「その後の工程」は、次のturn（revise・resume・回答の配送・差し戻し・`send_instruction`の依頼）、receiptを受けた後のvalidation、validationの後のreview、reviewの後のe2e、着地の開始。
- 走っている工程の結果（receipt、validationの受理か拒否、reviewのverdict、e2eの結果）は今のとおり記録する。
  結果が次の工程を求めていれば（例: reviewの`revise`）、その工程を始めずに待ちへ入る。
- backgroundのwrapperはturnの間も次の依頼を待って生きているので、待ちに入る前に、走っていたturnが終わった後でwrapperに終了を依頼し（今の終了の依頼の経路）、pidで終了を確かめる。
  終了を確かめるまでは`settling`のままで、確かめられなければ今の終了の時間切れの経路（pidで止める）に従う。
  待ちの間はrunのprocessが無く、書き手が居ない。
- 待ちに入ったrunはworkerの枠（`used_slots()`）を放す。
  [人の答えを待つrun](waiting.md)と同じくslotの外に置くが、`--max-waiting`と`waiting.count`には数えない（processを持たないので、上限の根拠のメモリを使わない）。
- 待ちの間もsessionとworktreeを残す。
  sessionはproviderの会話のIDで、解除で同じsessionから続けるときは今のresumeと同じく新しいwrapperで開き直す。
  worktreeは未commitの差分ごと残し、終わったrunの掃除（`sweep_ended_runs`）はholdの開いたtaskのrunに触れない。
- 走っていた工程が待ちに入る前に別の形で終わったとき（turnの失敗・中断、`recover`、turnの上限、`needs_session`へのpark、復旧jobの終わり、着地の終わり）も、その遷移の確定が同じtransactionで開いたholdを確かめ、次の工程を始めずに安全な待ちに入った時刻を書く。
  そうしないと`settling`が残り、解除が全て`hold_not_settled`で拒まれる。
- holdの待ちは、stall・receiptの無いidle・`idle_process`・wrapperの沈黙の検知と、`resume_timeout`・turnの上限の計時の対象にしない。
  待ちに入る前に走っていたturnの計時と検知は今のとおり続く（走っている工程を止めない）。
- supervisorが替わっても（execの引き継ぎ・adopt）、holdは表とeventから組み立て直し、待ちを記録し直さない。

## 開始の直前の再検査

- fill passが集めた候補（[ADR-t1850-1](../../adr/2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)の1つの列）は、holdの開いたtaskを列に入れない。
- それに加えて、候補を集めた後にholdがかかることがあるので、`fill_slots`が列の先頭から開始の処理を呼ぶ箇所（claim・`needs_session`のresume・復旧jobの開始）で、開始の確定のtransactionの中でholdを確かめ直し、開いていれば始めずに列の次へ進む。
  claimの確定（`ready → in_progress`）、resumeの`resume_started`、復旧jobの`triage_started`は、それぞれのtransactionで開いたholdが無いことを条件にする。
- 列の外で始まる進行（回答を次のturnとして届ける`deliver_answers`、reviewの後の差し戻しの送信、着地の開始`begin_integration`、人の答えを待つrunのslotへの戻り、待ちの最中に失ったsessionの開き直し`reopen::reopen_lost_session`）も、始める確定の中でholdを確かめる。

## 列の外の結果と回答の適用

次の3つは候補の列の外で動き、holdの前に始まったものの結果がhold中に届く。

- 終わったrunの復旧jobの`decide`のaskへの回答の適用（`triage::apply_triage_answers`）
- 終わったrunの復旧jobのverdictの適用（`triage::act_on_recovery`）
- 生きているrunの復旧jobのverdictの適用（`recovery::apply_live`）

hold中のtaskに対しては、どれも進行（taskをreadyに戻す、runを`needs_session`にする、指示を次のturnにする、processを止める、cancel）として適用しない。

- verdictと回答はそのまま記録として残し（`hold_result_kept`。どの適用か、verdictか回答の中身、記録したときのtaskとrunの版）、askは閉じない。
- jobのラウンドの記録（`triage_finished`・`recovery_finished`）は、適用しなかったことと理由（`held`）を持つ。
- 解除の後、最初のpassで残した結果を読み直す。
  taskとrunの版（taskのstatusとholdの外の変更、runのstatus、そのrunの進行を変えるeventの最後のID）が残したときと同じで、解除が続け方を変えていなければ、今の適用の経路（前提の再検査を含む）で適用し`hold_result_applied`を記録する。
  変わっていれば、または解除が新しいrunかproviderの変更を選んだときは、古いものとして捨て`hold_result_discarded`（理由つき）を記録し、askは理由を付けて閉じる。
- supervisorが配送か適用をする回答（`worker_question`、`stalled`への`stop`か指示の文、`approve_landing`など）も、hold中は配送と適用をせずに残し、解除の後に同じ規則で扱う。

## 安全な待ちと解除の順序

holdは2段で進む: holdを確定した後、走っている工程が終わるまでの`settling`と、待ちに入った後の`held`。

- 待ちに入る確定は、holdがまだ開いていることを同じtransactionの中で確かめ、表の安全な待ちに入った時刻と`run_phase_changed`（工程`held`）を書く。
  holdが既に解除されていれば待ちに入らず、今の工程の後の進行を今のとおり始める。
- 解除は、表の安全な待ちに入った時刻があることを同じtransactionの中で確かめる。
  `settling`の間の解除は、`--provider`・`--model`の有無にかかわらず理由`hold_not_settled`で拒み、holdの印とtaskのprovider・modelを変えず、resumeも新しいrunも始めない。
- どちらの確定も同じstoreの`BEGIN IMMEDIATE`で書くので、先に確定した側だけが効き、結果は1つに決まる。
  どちらの順でも、走っているturn・validation・reviewを止めず、sessionと成果のbranch・未commitの差分を捨てず、resume・新しいrun・着地を二重に始めない。
- `settling`が長く続く（走っているturnがturnの上限まで続くなど）ときも、解除は待たせて受けない。
  そのturnの終わり方（上限での終わりを含む）は今の経路のままで、終わった後に待ちへ入る。

## 解除の分岐

解除は安全な待ちに入った後だけ受け、選んだ続け方で再開する。

| holdの時の状態 | 既定の続け方 | provider・modelの指定 |
| --- | --- | --- |
| ready（runが無い） | claimの候補に戻る（`--new-run`は拒む） | `--provider`はtaskのworkerのproviderを変える。`--model`は次のclaimのsessionのmodelにする |
| runが待ちに入り、同じproviderでsessionが使える | 同じsessionで、止めていた工程から続ける（列の再開の候補としてslotを待つ） | `--model`だけなら同じsessionで、次のturnからそのmodelで開く |
| providerを変えたとき、sessionが無いとき（providerが会話を見つけない）、runが止まっている（`failed`・`interrupted`）とき、`--new-run`のとき | 成果を引き継ぐ新しいrun | 新しいrunのclaimに`--provider`・`--model`を使う |
| runが`needs_session` | 今のresumeの経路で続ける | providerを変えれば成果を引き継ぐ新しいrun、`--model`だけならresumeのsessionのmodel |

- 止まったrunに残した復旧jobの結果か回答があり、版が変わらず、解除がprovider・model・`--new-run`を選んでいなければ、新しいrunを作らずにその結果を適用する（上の「列の外の結果と回答の適用」）。
- 成果を引き継ぐ新しいrunは、今の引き継ぐretry（`retry_inherit`）と同じく、runの検証済みのcommit（無ければworktreeのHEAD、rebaseの途中なら検証済みのcommitだけ）を`refs/dagq/runs/<run-id>`に残し、taskをplan reviewを通さずにreadyへ戻して、次のrunのpromptの引き継ぎの節に載せる。
  branchに自分のcommitが無ければ、引き継ぐものが無いので最初から始める（捨てる成果が無い）。
- worktreeの未commitの差分は、新しいrunを始める前にruntimeが元のrunのrun directoryにpatchとして保存し、新しいrunのpromptの引き継ぎの節がその場所を示す。
  保存できなければ解除を拒み、holdを残す（成果を暗黙に捨てない）。
- 新しいrunを始める前に、元のrunのwrapperが居ないこと（安全な待ちに入るときに確かめたもの）をもう一度pidで確かめ、居れば解除を拒む（書き手を2つにしない）。
- 引き継ぎは`task_released`の`inherit`（`branch`・`head`・patchの場所）に記録し、次のrunの引き継ぎの判定（今は`triage_finished` / `triage_decided`の`action: retry_inherit`を読む`domain::resume`）がこれも読む。
  taskごとに1回の判定（`is_inherit_retry`）はこれを数えない。
- 元のrunは引き継ぐretryと同じ終わり方（`failed`）にするが、理由のコードで解除によるものと分かるようにし、復旧jobに回さず、retryの前提の失敗の数（`TRIAGE_RETRY_FAILURES`）にも数えない。
- 自動の`retry_inherit`の制限（taskごとに1回、自分のcommitのあるbranchに限る）は復旧jobの自動の判断の柵で、人の解除の新しいrunはそれを使わず、数えもしない。
  解除で新しいrunを作った後も、そのtaskの自動の`retry_inherit`はまだ1回できる。
- `--model`で選んだmodelは`task_released`の`model`と、その後にsessionを開くevent（`run_claimed` / `resume_started`と次のturn）の`model`に記録し、段上げの起点として読む。
  選んだmodelとeffortが段（`LADDER`）の値の1つならそこから段上げを続け、段に無い値なら今の段の外の値と同じく上げない。
- Codexのworkerは今modelを渡さない（Codexの既定のmodelで動く）ので、`--provider codex`か、Codexで続くrunへの`--model`は理由を付けて拒み、holdも選んだ値も変えない。
- 解除は優先度とclaimの順を変えず、再開・claimは今のとおり効く優先度の列でslotを待つ。

## 再計画の保留との共通の柵

- turnの境界で次の工程を止める柵は、[taskの再計画](task-replanning.md)の安全な保留（[ADR-t1521-2](../../adr/2026-10-05-t1521-2-trusted-runtime-parks-snapshots-and-atomically-replaces-tasks.md)決定1）と1つにする。
  柵は「次の工程を始めてよいか」を1つの判定で返し、保留の理由（再計画・hold）を並べて持つ。
- 先に実装された側の柵を使い、後の側はそこに理由を足す。
  holdは再計画のgenerationとsnapshotを使わず、計画を変えないので、plannerを起動しない。
- 再計画の保留とholdが同じtaskに重なれば、両方が外れるまで次の工程を始めない。
  holdの解除は再計画の保留を外さず、再計画の結末（適用・継続・撤回）はholdを外さない。

## CLI

- `dagq hold <TASK|RUN> --reason <text> [--until <time>]`: holdをかける。
  数字はtask、run IDはそのrunのtask。
  `--reason`は必須で空を拒む。
  `--until`は知らせる時刻で、過ぎても自動では外さない。
- `dagq release <TASK|RUN> [--provider <claude|codex>] [--model <model>] [--new-run]`: holdを外す。
  開いたholdが無ければ拒み、`settling`なら`hold_not_settled`で拒む。
- 出力は他の操作系のコマンドと同じJSONで、holdの状態（`settling` / `held`）と解除の続け方を返す。

## event

taskのevent（runを持てば`run_id`も付ける）。

- `task_held`: holdを確定した。
  payloadは`reason`・`until`・`requested`（名指したものがtaskかrunか）・そのときのrunのIDとstatus・`state`（`settling`か`held`）。
- `run_phase_changed`（工程`held`、`blocker: human`、`holds: none`）: runが安全な待ちに入った（[計測](../measurement.md)）。
  runの無いreadyのtaskは`task_held`の`state: held`が同じ時刻を表す。
- `task_hold_settled`: 計測の段1（`run_phase_changed`）より前に実装するときだけ、runが安全な待ちに入った時刻を表す（下の「計測とtimeline」）。
  段1の後は工程`held`の`run_phase_changed`に置き換える。
- `task_released`: holdを外した。
  payloadは`continuation`（`claim` / `continue` / `resume` / `new_run`）・`provider`・`model`・かけてからの秒と待ちに入ってからの秒。
- `hold_result_kept` / `hold_result_applied` / `hold_result_discarded`: 列の外の結果と回答を残した・適用した・捨てた。
- 解除の拒否（`hold_not_settled`、開いたholdが無い）は状態を変えないのでeventを書かない。
- 終端になったtaskのholdは`task_hold_ended`（理由`completed` / `canceled`）で閉じる。

## statusとattentionと知らせ

- `status`は、開いたholdを`task_holds`の配列に出す（taskとrunのID・理由・かけたactor・かけた時刻・待ちに入った時刻・`until`・状態）。
  runの`progress.phase`は待ちの間`held`、`slot`はslotの外。
- attentionは、`until`を過ぎたhold、`until`の無いholdはかけてから24時間（定数。後で`dagq.toml`の設定にできる）を過ぎたものを、`kind: task_held`・`next: release or keep the hold of task N`で出す。
  inboxが同じholdを繰り返し起こさないよう、知らせは24時間ごとに1回にする。
- `list`・`show`は、holdの開いたtaskに理由と状態を付ける。
- `candidates`は、holdの開いたreadyのtaskを候補から除き、除いたことを理由（task hold）つきで示す。
  queue全体の控えを出す今の`held`とは混ぜない。

## 権限

- `hold`と`release`はuserとinboxだけ（予定のcapability `task.hold`）。
  planner・worker・job・observer・supervisor・wrapperは`authorization_denied`で拒む。
- 実装の変更は同じ変更で[Authorization](../authorization.md)の表、[Security](../security.md)の「actor × capability」、pluginの権限の表を直す（[文書の規則](../../development/documents.md)の「権限の表を写す文書」）。

## 計測とtimeline

- holdをかけた時刻（`task_held`）と、走っていた工程が終わって実際に待ちへ入った時刻（工程`held`の`run_phase_changed`）を分ける。
  `settling`の間はrunの工程が今のとおり進むので、その区間は元の工程のタグのまま数える。
- 計測の段1（`run_phase_changed`）より前に実装するなら、待ちに入った時刻は表と`task_hold_settled`のeventに書き、段1で工程`held`に移す。
- 分析から除くのは、待ちに入った時刻から解除（`task_released`）までの区間（工程`held`、`blocker: human`、`holds: none`）。
  解除の後は、再開・claimの工程に移る。
- readyのtask（claimの前）のholdは、`task_held`から`task_released`までをtaskの区間に重ねて読む（[計測](../measurement.md)の「claimの前」）。
- [timeline](timeline.md)は、待ちに入ってから解除までの空白に理由`held`を付ける。
  `no_supervisor`の次、`waiting_ask`より前に判定する。

## 実装の入口（予定）

| 知りたいこと | コードの入口 |
| --- | --- |
| holdの判定と状態 | 予定の`domain::task_hold`（hold・解除・`settling`の判定、解除の分岐を値で受ける関数） |
| 開始の直前の再検査 | `application::supervise`の`fill_slots`と、claim・resume・復旧jobの開始の確定 |
| 列の外の適用 | `triage::apply_triage_answers`・`triage::act_on_recovery`・`recovery::apply_live` |
| 待ちへの出入り | `application::supervise::waiting`の待ちの印に並ぶholdの印 |
| 引き継ぐ新しいrun | `resume::inherited_head`と引き継ぐretryの記録 |
| 再計画と共有する柵 | [taskの再計画](task-replanning.md)の「停止境界と競合」 |
