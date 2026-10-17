---
id: design-supervisor-lifecycle-plan-planners
type: design
title: "`plan` / `planners`"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1704-1
  - adr-t1582-1
  - design-supervisor-lifecycle
  - adr-0044
  - design-persistence
  - design-supervisor-lifecycle-actor-model
  - adr-t803-1
  - adr-t1300-1
  - adr-t1394-1
  - adr-t1394-2
  - adr-t1228-1
  - adr-t1533-1
  - adr-t1404-1
  - design-supervisor-lifecycle-headless-worker
---

# `plan` / `planners`

> runtimeのplannerは非対話でbackgroundのwrapperだけで動き（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定3、[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）、どのplannerの画面も読まず、どのplannerにも打ち込まない（[runtimeのplannerの経路](#runtimeのplannerの経路)）。
> 人が開くplannerは[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)で廃止し、計画の入口はinboxからの依頼（[inboxからの計画の依頼](#inboxからの計画の依頼)）にした。
> 開いたままの人のplannerの行は、supervisorがcmuxを呼ばずに閉じた扱いにする（ADR-t1433-2決定5、下の「人のplannerの終わり」）。
> 開いているplannerへの続きの依頼は[ADR-t1533-1](../../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)のとおりplannerのIDで渡し（[続きの依頼と非対話のplannerのCLI](#続きの依頼と非対話のplannerのcli)）、plannerを閉じるCLIは作らない（同ADR決定2。runtimeのplannerはsupervisorが閉じる）。

[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定1・6・12・13。
plannerは常駐せず、proposalごと（と依頼・draft・findingごと）にオンデマンドで非対話のwrapperをbackgroundに起動する（workspaceは開かない）。
use caseは`src/application/planner.rs`。

- **記録**: planner 1つが`planners`表（[persistence](../persistence.md)）の1行。
  `session_workspaces`はroleが主キーでplannerを1つしか持てないので使わない。
  行はorigin（人かruntime）、runtimeが立てたときのproposal、session（runtimeのplannerはbackgroundのwrapperのhandle、人のplannerと古いバイナリが開いた対話のruntimeのplannerはworkspace）、wrapperとagentの生存の記録、終わった印、agentの経路を持つ。
  runtimeのplannerは開くときに経路に常に`headless`を書き、`interactive`は過去の行の履歴として読む。
  plannerのディレクトリは`<queue dir>/planners/<planner-id>/`で、初期prompt、wrapperとして動くバイナリの写し（`runner`。runの`runner`と同じく、ビルドし直しても動いているplannerは変わらない）、turnの依頼と出力（`turns/`）、logとidle marker、supervisorの入力の印を置く。
- **`dagq plan`**（[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)決定1）: 何も開かず、行も作らず、cmuxもClaudeも呼ばずに失敗し、errorに案内（`application::planner::PLAN_REFUSED`）を出す。
  引数は受け付けて無視し、同じ案内を出す。
  権限の検査（`planner.open`）は残るので、打てるactorは案内を、打てないactorは`authorization_denied`を受ける。
  人が`DAGQ_ROLE`の無いterminalで打つ`add` / `goal add` / `submit`は通る（決定8）。
- **plannerの開き方**（`launch_planner`）: 開くたびに新しいwrapperを起動するので、plannerは複数同時に開ける。
  行を作り、経路に`headless`を書き（`dagq.toml`に依らない。書けなければ行をerror付きで閉じる）、ディレクトリに残っているidle markerを消す（queue DBを作り直すとplannerのIDは1から振り直されるので、前のDBの同じIDのplannerが残したmarkerで新しいplannerが`idle`に見えないように）。
  prompt・turnの上限・`runner`を書き、wrapperをbackgroundに起動する（切り離したプロセスで、開くのにcmuxは要らない）。
  wrapperとそのturnの環境はplannerのroleとoriginとIDを持ち、wrapperのhandleを行のsessionに書く。
  wrapperが起動しなければ行を`error`付きで閉じてerrorで止まる。
  handleを行に書けなかったときは、記録の無いwrapperを残さないようにそのwrapperを止めて（[非対話のworker](headless-worker.md)の「停止の記録」）から行をerror付きで閉じ、止めたか・止めるのにも失敗したかをerrorに書く。
  **model / effort**（[Actor model](actor-model.md)、ADR-0079の決定7）: runtimeのplannerはsupervisorが読む`[roles.runtime_planner]`で決め、表が無ければmodelとeffortを付けない。
  reviseのために開くruntimeのplannerはeffortを1段上げる。
  決めた`launch`は結果と環境に入り、pluginのhookが記録するplannerの区間の`session_opened`の`launch`になる。
  下記の**行の片付け**はsupervisorだけが行い、`planners`は行を閉じない。
  `submit`はruntimeのplannerのturnからは環境のplannerのIDで引いたplannerの記録のhandleとoriginからproposalの持ち主を決めるので、plannerが出したproposalはそのplannerのものになる（wrapperは最初のturnの前に、開いた側がhandleを記録するのを待つ）。
- **runtimeが立てるplanner**: `open_runtime_planner`がproposalの存在を確かめ、originがruntimeでproposal付きの行でplannerを開く。
  初期promptは、proposal、そのtask、plan reviewの指摘（reasons）と記録を読むCLI（ADR-0044の決定22）と、verify・paths・evidenceを決める順（[Prompt](prompt.md#repositoryの規則を読む順)）を載せ、直してsubmitで出し直すこと、計画の意図を変える修正はinboxにaskして答えを待つことを指示する。
  reasonsとtaskの行は上限の中に収め（[Prompt](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)）、省いたreasonは最新のplan reviewの記録先のtaskのeventから、省いたtaskはproposalから読むと書く（reopenでは元のreviewのanchorを使い、該当eventが無ければ全文を読む既知の方法は無いと書く）。
  promptのbyte数は`planner_prompt_written`（[Prompt](prompt.md)）に記録する。
  runtimeのplannerとして出し直すので、そのplannerがproposalの持ち主になる。
  supervisorはplan reviewのrevise（持ち主のplannerが生きていないとき）とreopenでこれを呼ぶ（[Plan review](plan-review.md#reviseの配送)）。
- **session wrapper（`planner-session`、hidden）**: runの`session`と同じく、自分をplannerの行に登録する（1つのplannerに1回だけ。閉じたplannerは拒む）。
  runtimeのplannerのwrapperは下の「非対話のplannerのwrapper」で動く。
  対話のagentを起動して終わりまでheartbeatするのは人のplannerのwrapperだけで（[Agent provider lifecycle](../provider-lifecycle.md)）、terminalには何も出さず、workspaceを閉じない（ADR-t1300-1の予告と猶予はADR-t1433-2決定5で対象が無く、workspaceは人が自分のterminalで閉じる）。
  **登録を断られたwrapper**: 閉じた行か、別のwrapperが登録済みの行で登録を断られると、wrapperは断られたことをlogに書いてerrorで終わり、workspaceの中で動く古いwrapperもそのworkspaceを閉じない（cmuxを呼ばない）。
- **非対話のplannerのwrapper**: runtimeのplannerのwrapperは、agentを1つ起動する代わりに非対話のworkerと同じ`Turns`でturnを動かす（下の[runtimeのplannerの経路](#runtimeのplannerの経路)、[非対話のworker](headless-worker.md#非対話のruntimeのplanner)）。
  端末は要らず、終了の依頼か、sessionを終わらせたturnの後に`planner_exited`を記録する。
- **生存とidleの判定**（`planner_view`）: runtimeのplannerは画面を読まず、turnとmarkerで判定する。
  閉じた行か、sessionが居なければ`closed`（backgroundのhandleが居るかはそのプロセス（pidと記録した起動時刻）で見る）。
  cmuxのworkspaceを記録した行は、originに依らずcmuxに問わず`closed`と読む（ADR-t1433-2決定3・5）。
  sessionかwrapperの登録がまだなら`opening`、起動の猶予を過ぎてもそのままなら`lost`（wrapperが起動できなかった、開いたプロセスがsessionを記録する前に死んだ）。
  agentの終了が記録されていれば`exited`、wrapperのPIDが死んでいれば`lost`（heartbeatの古さで`lost`にするのはworkspaceのwrapperの行だけ）。
  wrapperが生きていてもagentのPIDをまだ記録していなければ、markerに依らず`opening`（agentの居ないplannerに依頼や文面を送らない。originに依らない）。
  それ以外は動いているsessionで、下の[runtimeのplannerの経路](#runtimeのplannerの経路)の「状態」で`idle`か`working`になる。
  どのplannerの画面も読まず、画面からのidleの推定は無い。
  `idle_since`はidleのときのmarkerの時刻で、送った文面より後にidleになったかの比較に使える。
  runtimeのplannerに届けた`planner_question`の答えを読み終えたかは、その答えの依頼を取ったturnで判じ（下の「状態」）、askが閉じた時刻とは比べない（askが閉じるのは配送の後なので、その前にidleになったplannerを待ち続けて枠をふさぐ）。
  上限で持ち主の居ないreviseが期限を過ぎて待つときは、何も待たないidleのruntimeのplannerに終了の依頼を置いて枠を空ける（[Plan review](plan-review.md#reviseの配送)）。
  人が開いたplannerには終わりを頼まない（行は下の「人のplannerの終わり」で閉じる）。
- **runtimeのplannerの時間切れ**: runtimeのplannerはturnの`[stall]`の上限で時間を切られ、wrapperが上限で止めたturnをsupervisorがplannerごとに1回`planner_unresponsive`で知らせる（下の[runtimeのplannerの経路](#runtimeのplannerの経路)の「状態」）。
  planner timeoutは、plannerに渡したreviseとplannerを待つreviseの時間切れ（proposalの`planner_unresponsive`）と、枠をふさぐidleのplannerの解放に使う。
  plannerの`planner_unresponsive`はqueueのevent（task・goal・runに付かない）で、`status`のattentionにはそのplannerの行が閉じるまで出て、draftのplannerならdraftのtaskに付く。
  人が開いたplannerは対象にしない。
- **人のplannerの終わり**（`close_person_planners`、[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定5。ADR-t1394-1決定1・7・9をamends）: 人が開いたplannerで閉じていない行を、生きていても終わっていても閉じ、`planner_closed`（`person_retired`、workspaceは閉じない。ADR-t1300-1決定2のevent）を1回記録する。
  cmuxは呼ばない: そのworkspaceを一覧で探さず、`send`も`close`もせず、画面も読まない。
  残ったworkspaceは人が`DAGQ_ROLE`の無い自分のterminalで閉じる（理由の文にもそう書く）。
  行うのはsupervisorのplannerのpassの初め（`--no-claude`でも。そのpassを行わない回、つまり止める途中・claimを止めた間・queueの控えの間は行わない）。
  行を閉じた後は、そのplannerが持っていたproposalのreviseと`planner_question`のanswerは、持ち主のplannerが閉じているときの経路（runtimeが新しい非対話のplannerを立てる。ADR-0047決定12、ADR-t1394-1決定7）で届く（reviseの持ち主の探し方は閉じていない行だけを見る。answerの行き先はもともとruntimeのplannerだけを選ぶ）。
  ADR-t1300-1の猶予・cmuxの一覧と`close`・wrapperの予告と、人のplannerへの打ち込みは無い。
  下の行の片付けは人のplannerの行を閉じず、ここに任せる（どの行も`person_retired`の1つのcodeで閉じるため）。
- **plannerの終わりの記録**（`planner_closed`、ADR-t1300-1）: runtimeがplannerの行を閉じるとき、originに依らず、行を閉じるのと同じトランザクションでqueueのevent（task・goal・runに付かない）`planner_closed`を1回記録する（行がすでに閉じていれば何も記録しない）。
  payloadは`planner_closed_payload`が作り、画面の中身は載せない。
  `code`は上の人のplannerの終わり、runtimeのplannerの終わり方（[Plan review](plan-review.md#runtimeのplannerの終わり)。古いバイナリがworkspaceに開いた対話の行も、cmuxを呼ばずにsessionが居ないとして閉じ、workspaceは人が閉じるよう理由に書く）、人の答えだけを待って終了の依頼を受けた行（下の[runtimeのplannerの経路](#runtimeのplannerの経路)）、下の行の片付けを分ける。
  wrapperを起動できずに`error`付きで閉じる行は、このeventを記録しない。
- **行の片付け**（`close_abandoned_planners`）: 閉じていないruntimeのplannerの行のうち、sessionが記録されていて、それが居らず、wrapperが終わっている行を（`error`なしで）閉じ、`planner_closed`（`abandoned`、workspaceは閉じない）を記録する（人のplannerの行は上の「人のplannerの終わり」に任せる）。
  閉じた行は`planners`に出なくなる。
  wrapperが終わっているとは、agentの終了が記録されている、wrapperのPIDが死んでいる、heartbeatが古い（workspaceの行だけ）、のどれかで、wrapperがまだ登録されていない行は起動の猶予を過ぎたとき。
  sessionが居ないだけでは閉じない（wrapperが生きている行と、まだ起動の猶予の中の行は残す）。
  cmuxの一覧は取らない（ADR-t1433-2決定3・5）: backgroundのhandleはプロセス（pidと記録した起動時刻。wrapperが残したturnが走る間も真で、判じられなければ居るものとして閉じない）で判じる。
  agentの終了が記録された行と人の答えだけを待って終了の依頼を受けた行は、passと同じ`code`で閉じる。
  古いバイナリがcmuxのworkspaceに開いたruntimeのplannerの行は、cmuxに問わず居ないものとして扱い、workspaceは閉じない。
  行うのはsupervisorのsweep（失敗はlogだけ）。
  runtimeのplannerは生きている間はsupervisorのpassが終了の依頼を置き、wrapperが終わってから（時間切れならbackgroundのwrapperを止めてから）行を閉じ、片付けはsupervisorが止まっていた間にwrapperが消えた分も拾う。
- **`dagq planners`**: 閉じていないplanner（全部も選べる）を古い順に、行に状態・生存・`idle_since`・ディレクトリ（backgroundのwrapperのhandleを記録したplannerならhandleとlogも）を足して返す（cmuxも画面も読まない）。
  読むだけで、observerとreviewerも打てる（`plan`と`planner-session`は拒む）。
- **`runner`の片付け**: plannerの`runner`はwrapperが動いている間しか使わない（plannerは同じディレクトリで開き直さない）。
  wrapperはagentの終了を記録した後（agentが起動できなかったときも）に自分の`runner`を消す（動いているプロセスのイメージは残るので、消しても走り続ける）。
  消せなかった分と、wrapperが終了を記録せずに死んだ分は、`remove_unused_planner_runners`が閉じた行も含む全行を見て消す（同じpidでも起動時刻が違うか読めなければ別のプロセスがpidを取ったとみなし、backgroundの行はheartbeatの古さでは消さない。ADR-t1404-1の決定2）。
  行が閉じているだけでは消さない（wrapperが生きている間は残す）。
  行とディレクトリの他のfileは残す。
  行うのは上の行の片付けと同じ回（supervisorのsweep。失敗はlogだけ）。

## runtimeのplannerの経路<a id="runtimeのplannerの経路"></a>

[ADR-t1394-2](../../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)の決定1・2。
[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定3により対話の経路は無く、[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)決定1・2によりwrapperはbackgroundだけで動く。

- **経路の欄**: 選ぶ経路は無く、runtimeのplannerは非対話だけ。
  supervisorはplannerを開くたびに行の経路へ`headless`を書く（書けなければ行をerror付きで閉じる）。
  `[roles.runtime_planner]`の旧keyの扱いは[Actor model](actor-model.md#runtimeのplannerの経路)が持つ。
  `[headless] wrapper`はplannerには読まない（backgroundだけ）。
  `planners.route`は過去の対話のplannerを読むために残す。
  人が開いたplannerの行は対話のままで、supervisorがcmuxを呼ばずに閉じる（上の「人のplannerの終わり」）。
- **非対話のplanner**: plannerのディレクトリに`turns/`（[非対話のworker](headless-worker.md#run-dirのturns)と同じ形）を置き、wrapperが非対話のworkerと同じ`Turns`で動く。
  初期promptが最初のturn。
  turnのeventと、supervisorが書く`turn_requested`はqueueのevent（plannerのID付き）。
  reviseの配送、`planner_question`のanswer、Claudeが使えなかったturnの続き（下の「使えないとき」）、plannerの終わりと枠の解放の終わりの依頼は、supervisorが次のturnの依頼と終了の依頼にし（下の「turnの依頼の経路」）、打ち込み・Stop hook・画面からのidleの推定を使わない。
  wrapperはworkspaceを持たずbackgroundだけで動く（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)、ADR-t1433-3）。
  backgroundのhandleは行のsessionに入り、`dagq submit`はplannerの記録からそのhandleを持ち主にする。
- **状態**: 非対話のplannerは、wrapperがturnの終わりに書くidle markerが最後の入力（supervisorの入力の印を含む）より新しく、`turns/`に取られていない依頼が無ければ`idle`、そうでなければ`working`、wrapperが死ねば`lost`。
  ただしmarkerのturnがClaudeのログイン切れ・利用上限・起動の失敗で終わっていれば、取られていない依頼が待っていても`idle`（wrapperはその依頼を`provider retry`の後まで取らないので、下の「使えないとき」が見る）。
  届けたanswerを読み終えるまでと、answerを届ける前は終了の依頼を置かない（人の答えだけを待つものは下）。
  届けたanswerを読み終えたかは、idle markerと配送の秒の比べでなく（同じ秒のうちに終わったturnの後も待ち続ける）、そのanswerの依頼を取ったturnが壁で失敗せずに終わったかで判じ、壁で失敗したturnだけでは読み終えたとせず、次の`provider retry`のturnが壁で失敗せずに終わるまで追う（判断は`domain::turn::request_read`。壁で失敗したturnの後にplannerを終わらせると、answerが読まれずに失われるため）。
  依頼の記録が無いときは、配送を取った秒とidle markerの秒の比べ。
  `planner_unresponsive`はplanner timeoutでなく、wrapperがturnを`[stall]`の上限で止めたときに、plannerごとに1回出る。
  止めたturnでsessionは終わるので、plannerはagentの終了で閉じられ、決めずに終わったplannerに数える。
  掃除がpassより先に行を閉じたときも、掃除が同じ知らせを出す（ADR-t1394-2決定3）。
  idleで仕事が終わらないまま期限を過ぎるものは、reviseを渡されたplannerのreviseの時間切れ（proposalの`planner_unresponsive`）が見る。
- **人の答えだけを待つplannerの終わり**（[ADR-t1704-1](../../adr/2026-10-05-t1704-1-human-answer-wait-releases-runtime-planner-slots.md)決定1・2・6）: idleの非対話のplannerを引き止めるものが、自分の未回答の`planner_question`と、それと並んで渡されたreviseだけなら、その場で判じ直し、askがまだ未回答であることと同じトランザクションで行の印とqueueのevent `planner_answer_wait`を書いてから終了の依頼を置く（askは閉じない）。
  行は`planner_closed`で閉じ、枠が空く。
  answerの依頼を書けなかったplannerも、最後のturnの後に書いた依頼と読み終えていないanswerが無くなってから、そのaskが回答済みで閉じていないことと同じトランザクションで同じ印を付け、終了の依頼を置く。
  終了の依頼も書けなければ、終了の時間切れの後にwrapperを止めて行を閉じる。
  印の付いた行はanswerの行き先から外れ、answerは行が閉じてから新しいplannerの初期promptで1回だけ運ばれ、前のplannerのnoteと作った・編集したdraftも載る（sessionはresumeしない。[Prompt](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)）。
  閉じた行は応答しないplannerの検出から外れ、決めずに終わった回数に数えない。
- **終わり**: 終了の依頼でwrapperが終わると、backgroundではhandleも無くなる。
  agentの終了が記録された行は、supervisorのpassと行の片付けのどちらが閉じても同じ`code`。
  行の片付けはbackgroundのhandleをcmuxの一覧でなくプロセス（pidと起動時刻）で判じ、生きているwrapperの行はheartbeatが遅れても閉じない。
  wrapperが殺されて残ったturnは、handleを閉じるときに止める（[非対話のworker](headless-worker.md#非対話のruntimeのplanner)）。
- **区間**: kind `runtime_planner`の区間を最初のturnから開いて`planner_closed`で閉じ（[非対話のworkerの区間](../provider-lifecycle.md#非対話のworkerの区間)と同じくturnから稼働時間とtokenを取る）、区間に経路を持たせる。
  `stats`と`kpi`は経路ごとに読み、経路は過去の対話の区間と比べるために残す。
- **使えないとき**（ADR-t1394-2決定5、`tend_planner_walls`）: idleの非対話のplannerの最後のturnがClaudeのログイン切れ・利用上限・起動の失敗で終わったなら、plannerを終わらせず（終了の依頼も解放もしない）、決めずに終わったplannerにも数えない。
  最初に見たとき、ログイン切れと利用上限はqueueの控えのaskを開くか加わり、起動の失敗はClaudeの`provider_held`を記録し、queueのevent `provider_waiting`を残す。
  控えのあいだはsupervisorのplannerのpass自体が動かない。
  Claudeの控えが解けたら、失敗したturnが取った依頼を載せた続きの依頼（workerと同じ文、`provider retry`）を次のturnとして置く。
  失敗したturn自体が`provider retry`だったときは、その前のturnへ遡って最初に壁で失敗した依頼を載せる（続きの続きを入れ子にしない）。
  壁の前（失敗したturnの途中）に置かれた依頼（revise・answer・続きの依頼）は、非対話のplannerのwrapperが`provider retry`より先に取らず（workerのwrapperは番号の順に取る）、止まったままのClaudeに対してturnを走らせない。
  plannerは壁で失敗したturnのmarkerのままidleに見え（上の「状態」）、控えのaskか`provider_held`と`provider_waiting`が残る。
  続きのturnが終わった後に、待っていた依頼を番号の順に取るので、失敗した依頼も後の依頼も取りこぼさない。
  待つあいだはreviseとanswerもそのplannerに置かず（失敗した依頼が後の依頼に隠れて続きに載らなくならないように）、続きのturnが終わった後に置く。
  turnの後に置かれた依頼でも`provider retry`でないもの（supervisorが止まる前に置いたものなど）は続きにならず、`provider retry`の後に取られる（続きが置かれたかは`provider retry`の`turn_requested`だけで判じる）。
  providerの切り替えは無い。
  `--no-claude`ではplannerを立てない。
- **turnの依頼の経路**: supervisorが非対話のplannerに置く依頼はすべて`headless::send_to_planner`を通り、どのplannerにも打ち込まない（非対話でない行、つまり古いバイナリがworkspaceに開いたruntimeのplannerと人のplannerの行はerrorで拒み何も打たない）。
  runの依頼と同じ書き方で、`turn_requested`をqueueのeventに残し、終了の入力は終了の依頼になる。
  依頼の前に入力の印を書く（終了の依頼を除く。`provider retry`も書く前に印を書く）。
  wrapperもplannerの依頼を取るたびに、`turns/`から取り出す前に入力の印を書く（turnの途中に置かれた依頼を取ったとき、前のturnのidle markerでidleに見えないように）。
  壁で失敗したturnの後に印だけが新しくなったとき（壁の前のviewから置いたanswerやreviseの印が失敗の後に届いた、`provider retry`の印を書いた後に依頼の書き込みが失敗した）は、`provider retry`が待っていなければ、wrapperが壁で失敗したturnのidle markerを書き直してidleに戻す（wrapperは`provider retry`より先に依頼を取らないので、そのままでは`working`に見えたまま誰も壁を見ない）。
  続きの依頼のCLI（`planner request`、下の[続きの依頼と非対話のplannerのCLI](#続きの依頼と非対話のplannerのcli)）はsupervisorを通らず、同じ書き方で置く（入力の印は書かない。取られるまでは上の「状態」で`working`で、今のturnがClaudeのログイン切れ・利用上限・起動の失敗で終わったときは`provider retry`の後まで取られず`idle`に見える）。
  依頼の書き込みはplannerかrunのディレクトリのlockを取ってから次の番号を決めるので、supervisorとCLIが同時に書いても番号はぶつからない。

### 続きの依頼と非対話のplannerのCLI<a id="続きの依頼と非対話のplannerのcli"></a>

[ADR-t1533-1](../../adr/2026-10-03-t1533-1-follow-up-requests-go-to-headless-planners-by-planner-id-and-no-planner-close.md)（ADR-t1228-1決定2・3、ADR-t1394-2決定7をamends）。

- **続きの依頼**: `dagq planner request`（`src/application/planner_request.rs`の`request_planner`）。
  人の言葉の入力は`request add`と同じ入力の経路（下の[inboxからの計画の依頼](#inboxからの計画の依頼)の1の「入力の経路」）で読む。
  宛先はplannerのID（依頼の記録のIDではない）。
  人の言葉をplannerのディレクトリの`requests/`の下の新しいファイルに書き（下の「依頼の言葉の受け渡し」と同じ）、それを指す決まった文を次のturnの依頼として置く。
  名前を決めて書き依頼を置くまではplannerのディレクトリのlockを取るので、同時の2つの依頼は別のファイルになる。
  置いた後に終了の依頼が置かれていれば、まだ取られていない依頼を消して拒む（取られない依頼を成功と返さない）。
  作業中のturnがあっても拒まず、依頼は番号の順に取られるので今のturnの後に届く。
  今のturnがClaudeのログイン切れ・利用上限・起動の失敗で終わったときは、`provider retry`が先に取られ、依頼はその後に届く（上の「使えないとき」）。
- **拒む条件**（理由付き、何も書かずcmuxも呼ばない）: 読めない・空の入力は、どの入力かと理由を書いたerrorで、plannerを見る前に拒む（何も残らない）。
  権限の検査は入力を読む前で、拒否の記録もそこに1回だけ残り、開いたstdinも待たない。
  人が開いたplanner、対話のplanner（cmuxへの送信は作らない）、閉じた行は行だけで拒み、ほかはplannerの状態で拒む。
  最後のturnがClaudeのログイン切れ・利用上限・起動の失敗で終わってidleのまま待つplannerも拒む（受けても続きの後まで待たされるうえ、控えのあいだに依頼を積まないため）。
  開いたplannerの居ない依頼に人の言葉を足すときは、新しい依頼を`request add`で記録する。
- **記録**: 成功するとqueueのevent `turn_requested`（supervisorの依頼と同じ形）と`planner_request_handed`を、どちらも呼び出し元をactorにして残す。
  依頼の文は`planner_request_handed`に載せない（fileを指す）。
  attentionにはしない。
- **判定**: capability `planner.request`（[Authorization](../authorization.md)）。
- **`planner screen` / `planner send`**: `planner screen`はどのplannerの画面も読まずcmuxも呼ばず、画面の代わりに理由と`turns/`の場所を返す（何も読まないので`screen_read`は残さない。知らないplannerはqueueのerror）。
  `planner send`はどのplannerにも、何かをする前に拒む（cmuxを呼ばず、配送を取らず、何も残さない）。
  非対話のplannerには画面もキーもanswerも無く答えはsupervisorが次のturnとして届けると、ほかには何も打ち込まないので`answer`で答えるよう案内する。
  capabilityの`screen.read` / `screen.send`と打てるactorは[Authorization](../authorization.md)が持つ。
- **backgroundのplannerの出力**（ADR-t1404-1決定6・8）: backgroundのwrapperはturnの要約をplannerのディレクトリのlogに書き、`dagq planner log`がplanner IDからそのlogを出す（閉じたplannerも）。
  workspaceの中のplannerはlogを持たず、画面はもう読まないので自分のterminalでそのworkspaceを読むよう案内して拒む。
  詳細は[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)の「logを読むCLI」。
- **plannerを閉じるCLIは無い**（ADR-t1533-1決定2）: runtimeのplannerは、仕事の終わり・決めずに終わったとき・時間切れ・行の片付けでsupervisorが閉じる。
- **画面の読み取りと送信は無い**: 画面を持つruntimeのplannerは無く、plannerの出力は上の`planner log`で読む（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)決定4、[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)決定5）。

## inboxからの計画の依頼<a id="inboxからの計画の依頼"></a>

[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)の決定2〜7と10。
人がinboxに計画を頼むと、inboxは人の言葉を依頼として記録し、supervisorが依頼ごとにruntimeのplannerを立て、plannerがsubmitか理由付きの却下まで進める。
人が要る判断は`planner_question`だけでinboxに上がる。
use caseは`src/application/supervise/request_planner.rs`で、記録は[Persistence](../persistence.md#planning-requestsplan_requestsrs)、規則は[Domain model](../domain-model.md#planning-requests)が持つ。
run slotは使わず、supervisorのplannerのpassの中で、reviseの配送と`planner_question`のanswerの配送の後、[Draft planners](draft-planners.md)と[Finding planners](finding-planners.md)の前に進める（人が待っているので先にする）。

1. **記録**（`dagq request add`）: 人の言葉（inboxの要約でなく人の書いた文）、inboxが補う文（人の言葉と分けて持つ）、元の通知の参照（任意で複数）、人の言葉にあった優先度（言葉に無ければ付けない）を`open`の依頼として記録し、queueのevent `request_recorded`を残す。
   記録できるのはinboxと`DAGQ_ROLE`の無い人だけで（capability `request.record`）、他のactorは`authorization_denied`を残して拒まれる（[Authorization](../authorization.md)）。
   inboxの記録は人の言葉の代行で、依頼者とeventのactorが`inbox`。
   依頼の中身は記録の後に変えず、言い直しは新しい依頼にする。
   - **拒む順**: CLIが同じcapabilityで、入力の検査とファイル・stdinの読み込みより先に権限を検査する。
     権限を持たないactorは、無いファイルでも入力のerrorにならず、開いたstdinも待たずに拒まれる。
     拒否を記録して直ちに返すので`authorization_denied`は1回だけで、依頼は残らない。
     applicationの記録（`request add`のuse case）も同じ権限を検査する。
   - **入力の経路**（`src/main.rs`の`request_words`）: 人の言葉は文字列・ファイル・stdinのちょうど1つで渡す。
     長い言葉・複数行・引用符を含む言葉がシェルのクォートで崩れないための経路で、inboxの補足も同じ形で渡す。
     言葉と補足の両方をstdinから読むことはstdinを取り合うので理由付きで拒む。
     記録するのはファイルのパスではなく読んだ時点の中身で、後でファイルが変わっても記録は変わらない（ADR-t1394-1決定2）。
     読めない・空の入力は、どの入力かと理由を書いたerrorで拒み、依頼を記録しない。
     開いているplannerへの`planner request`も同じ経路で読む。
   - **優先度**（[ADR-t1975-1](../../adr/2026-10-08-t1975-1-runtime-records-origin-and-person-set-priority-and-ai-actors-cannot-change-it.md)決定1・2）: plannerは書き写さない。
     依頼のplannerが作るgoalにruntimeが依頼の値を人の出どころで付け、`goal add`の別の値は依頼の値を名指して拒む。
     その依頼のtaskは、goalが同じ値を人の出どころで持たなければ（goalが無いときも）同じ値を個別に持つ。
     規則と由来の記録は`domain::plan_request`が持ち、由来は取り下げと出し直しでは変わらない。
2. **一覧**（`dagq requests`）: 古い順に`open`の依頼（全部や1件も選べる）を出す。
   読み取り（`queue.read`）で、plannerとobserverも打てる。
3. **立てる**（`open_request_planners`、各pass）: 対象は`open`で、閉じていないruntimeのplannerも閉じていない`planner_question`も無く、参照するtaskのどれにも閉じていないruntimeのdraftのplanner（再検討の時刻で立ったものを含む）が無い依頼（ID順）。
   同じdraftに依頼と再検討のplannerを同時に立てず、逆に`open`の依頼が参照するdraftには再検討のplannerを立てない（[Draft planners](draft-planners.md#再検討の時刻)、[ADR-t1540-1](../../adr/2026-10-05-t1540-1-a-kept-draft-returns-to-runtime-planners-at-its-revisit-time.md)）。
   runtimeが立てたplannerで生きているものが上限（[Draft planners](draft-planners.md)の3と同じ値。revise・draft・findingのplannerと枠を共有し、人が開いたplannerは数えない）未満のあいだ、依頼ごとに立てる。
   storeは書き込みのトランザクションで対象の条件を確かめ直してから行と`request_planner_opened`を書くので、2つのsupervisorや同じsupervisorの次のpassが同じ依頼に2つ立てない。
   plannerはbackgroundに起動した非対話のwrapperで、workspaceを持たない（上の[runtimeのplannerの経路](#runtimeのplannerの経路)）。
   `--no-claude`の間とplannerのpassを行わない回（止める途中・claimを止めた間・queueの控えの間）は立てず、依頼は`open`のまま待ち、`requests`に出る。
4. **依頼の言葉の受け渡し**（`planner_handoff::hand_request_to_planner`、[ADR-t1228-1](../../adr/2026-10-02-t1228-1-inbox-and-planner-reach-sessions-through-the-dagq-cli.md)の決定2）: 人の言葉はplannerのディレクトリの`requests/`の下のファイルに書き、promptはそれを決まった文で指す。
   依頼の本文をキー列として打たない。
   開いている非対話のruntimeのplannerへの続きの依頼（`planner request`、上の[続きの依頼と非対話のplannerのCLI](#続きの依頼と非対話のplannerのcli)）は同じ関数でファイルを書き、同じ文を次のturnの依頼として置く。
5. **初期prompt**（`request_planner_prompt`）: 依頼のファイルを指す文、inboxの補足（人の言葉と分けて）、参照先の中身（読めない参照はそう書く）、参照から引けるgoal（どれも引けなければgoalの一覧を読む手順）、前のplannerの`planner_question`とanswer、AGENTS.md（無ければCLAUDE.md）と名指す文書とrepositoryの規則を先に読むこと、既存のtaskと実装済みを確かめる手順、記録を読むCLI、[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)の上げる基準と3択（計画して`--context`の冒頭を`from request <id>`にしてsubmit、理由付きの却下、決めきれないものだけ依頼の`planner_question`）を載せる。
   人のanswerを運ぶplannerには元の質問とanswer（と前のplannerの`handover`）を載せる。
   各部と全体は上限の中に収め（[Prompt](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)）、省いたものは件数と、参照の種類ごとに読むコマンドを書く。
   promptのbyte数は`planner_prompt_written`に記録する。
6. **結末**: plannerが自分のturnから`dagq submit`すると（持ち主はplannerのIDで引いたplannerの記録のbackgroundのhandle）、submitが同じトランザクションでproposalを依頼に結ぶ（1件の依頼から複数のproposalを出してよい）。
   最初のsubmitで依頼は`proposed`になり、`request_proposed`を記録する。
   その後はproposalの通常の流れ（plan review、revise、concernの`approve_plan`のask）で進み、依頼の状態はproposalの結末で変えない。
   手当てしないと決めたら、plannerは`dagq request decline`で`declined`にし、`request_declined`を記録する（capability `request.decline`。打てるのはその依頼に立てられたplanner自身だけで、他のplanner・inbox・人は拒まれる。`open`でない依頼は拒む。判定に使った依頼のplannerが却下のトランザクションの中で変わっていれば拒む）。
   決めずに終わったplanner（sessionが終わった、idleで終了の依頼を受けた）の依頼は次のpassで再び対象になり、人の答えだけを待って終わったものを除くその依頼のplannerの数が依頼ごとの上限に達していれば（人のanswerを運ぶとき以外は）立てずに`exhausted`にして`request_planner_exhausted`を記録する。
   inboxの`watch`は`request_proposed`を知らせとして、`request_declined`と`request_planner_exhausted`をattention（人が言い直すか取り下げる）として受け、依頼した人に結末を伝える。
   どれもqueueのevent。
   出すtaskかgoalが指す前のproposal（accepted以外）が依頼に結ばれていれば、誰のsubmitでも、依頼が後で`declined`・`exhausted`でも新しいproposalを結ぶ（ADR-t1971-1決定2）。
7. **`planner_question`**: `ask --request`は`planner_question`だけが持て、task・run・findingを名指さない。
   plannerが開けるのはその依頼の閉じていないruntimeのplanner自身だけで、別の依頼・draft・finding・reviseのplannerは`authorization_denied`を残して拒まれ、askは開かない（`request decline`と同じ規則、[Authorization](../authorization.md#対話と記録のコマンドapplication)）。
   askは依頼を名指し、openなaskを1件に限る規則が依頼ごとに分かれる（[Persistence](../persistence.md#asks)）。
   answerの行き先（`planner_answer_route`）は、その依頼の閉じていないruntimeのplannerがあればそのplanner（[Draft planners](draft-planners.md)の5と同じく、plannerが自分で作ったaskはaskの後にidleになってから、他の人が作ったaskはidleなら、配送を取ってから次のturnの依頼にし（打ち込まない）、閉じる。
   配送の失敗はqueueのevent `ask_delivery_failed`を残し、plannerを終わらせてanswerを新しいplannerが運ぶ）。
   居なければ依頼がまだ`open`なら上限の中でanswerを持った新しいplannerを立てて渡し（依頼のplannerの数の上限を越えて運ぶ。立てる前に同じトランザクションでaskが回答済み・未closeであることも確かめ直し、閉じられたanswerでは立てない）、`open`でなければ（submit・却下・上限の後）閉じる。
   人には行かない。
   answerの`ask_answered`はattentionにしない。
   依頼のaskの配送のeventはqueueのevent。
   届けたanswerをまだturnで読み終えていないあいだは、そのplannerに終了の依頼を置かない。
   人の答えだけを待って終わるplanner（上の「人の答えだけを待つplannerの終わり」）にはanswerを渡さず、answerはその行が閉じるのを待つ。
8. **終わりと時間切れ**: 依頼のplannerは、draft・findingのplannerと同じく、仕事を終えてidleになるか人の答えだけを待てばsupervisorから終了の依頼を受け、`planner_closed`を残す。
   時間はturnの上限だけで切られ、turnが`[stall]`の上限で止められれば、plannerごとに1回`planner_unresponsive`がinboxに届く（上の「runtimeのplannerの時間切れ」と[runtimeのplannerの経路](#runtimeのplannerの経路)の「状態」）。
   plannerは、submit・却下・決めずに終わったことをturnの終わり（idle marker）とwrapperの終わりから同じ規則で判じ、Claudeが使えないあいだは待って回数に数えない（同じ節の「使えないとき」）。
   `dagq planners`の行は依頼を持つ。

### Spikeの再計画の依頼<a id="spikeの再計画の依頼"></a>

[ADR-t1487-1](../../adr/2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)の決定5・6・8。
Spikeの区分のtaskが`completed`になると、runtimeが起点のSpikeを持つ依頼を記録し、上の3〜8の経路でruntimeのplannerが結果から再計画する。
記録と照合は`infrastructure::plan_requests`が、promptは依頼のplannerの初期promptのSpikeの分岐が持つ。

- **記録**: 着地の`completed`への遷移と同じトランザクションで、Spikeごとに1件だけruntimeを依頼者として記録する。
  各passで依頼のplannerを立てる前に、依頼の無い`completed`のSpikeを照合して補う（古いバイナリの着地・トランザクションの失敗）。
  どちらもSpikeに依頼が無いときだけ書くので、再起動・execの引き継ぎ・2つのsupervisor・重複配送で欠けも二重もない。
  `canceled`のSpikeには作らない。
- **立てる**: 上の3と同じ枠で立ち、workerの枠を使わず、workerも待たない。
  同じgoalのSpike起点の依頼は、そのgoalの別の依頼のplannerが開いているあいだ待つ（goalを並行して再計画しない）。
- **prompt**: 人の言葉の代わりに、Spikeの着地したreceiptの`spike_result`と`follow_ups`、goalの他のSpikeの結果を載せる。
  証拠の条件（対象のcommitとmainの差、toolの版）を確かめる手順と、実装へ進む・上限付きの追加のSpike・代替案・範囲の縮小・中止（`request decline`）の5つの結末を載せる。
  receiptの`follow_ups`はdraftになってそのplannerが手当てするので、依頼のplannerは同じ手当てを二重に計画しない。
- **知らせ**: 頼んだ人が居ないので、Spike起点の`request_proposed`と`request_declined`はattentionにせず、`request_planner_exhausted`だけをattentionにする。
- **周期の上限**: `[goals] max_spike_cycles`（[Run environment](run-environment.md)）に達したgoalのSpikeは、runtimeのplannerのsubmitが拒み、plannerはdraftのまま`planner_question`で人に上げ、人のsubmitが通す。
  周期は、goalが調査中の計画（[claimを控える](claim-hold.md#調査中の計画の上限)）になってから終わるまでを1回と数える。
  runtimeは未解決を理由にSpikeを延長も連鎖もしない。

## 予定: inboxからの依頼と非対話のplanner<a id="予定-inboxからの依頼と非対話のplanner"></a>

[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)の予定のうち、まだ実装していないもの。
実装したらこの節を今の姿に書き直す。

- `status`のinbox向けの欄に、plannerを待っている`open`の依頼を出す（`--no-claude`の間に待っている依頼を人に見せるため。ADR-t1394-1のConsequences）。
  今は`dagq requests`で読む。

## taskの再計画の依頼

長期化した未達runの置換依頼も既存の永続requestとruntime plannerを使う。
人の`replan request`は再計画の操作に結んだ依頼を記録し、立てる対象（3）はその操作が完全なsnapshotを持ってplannerの段に入るまでその依頼を外す（`plan_requests.rs`の`TARGETS`）。
残りの予定は[taskの再計画](../../plans/task-replanning.md)が持つ。
