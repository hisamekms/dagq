---
id: design-supervisor-lifecycle-queue-hold
type: design
title: "認証と利用上限のaskの待ちとanswer"
status: current
created: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle-task-hold
  - adr-0047
  - adr-t813-2
  - adr-t1063-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-ask
  - design-supervisor-lifecycle-claim-hold
  - design-supervisor-lifecycle-disk-space
  - design-supervisor-lifecycle-waiting
  - design-supervisor-lifecycle-status
---

# 認証と利用上限のaskの待ちとanswer

Claude Codeのログインが切れるか利用上限に達すると、新しいrunもheadlessのjobも同じ理由で止まる。runtimeはそれをqueueで1件の`queue_hold`のask（`authentication`、または`cost`で`subject: usage_limit`）にまとめ（task 361・438、下の「検知」と[ask](ask.md)）、openな間は新しいclaimとheadless jobの起動を控え、人のanswerを自分で適用する（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定42の「待ち」と「answer」、task 437）。ディスクの`cost`のask（`subject: disk`、options `done` / `wait`）は[空き容量を確かめる](disk-space.md)（task 377）が扱い、ここの控えとanswerの適用には入らない。

taskを1つずつ待たせるtaskのhold（予定・未実装）は、この控えと別の層で[taskのhold](task-hold.md)が持つ。

## 検知（task 438）

文言の型は`infrastructure::claude`の1か所に置き、turn の TurnFailure と `AgentSignals::job_failure`で読む。headless jobの失敗は共通の分類`JobFailure`（[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)、task 1064）で返り、`authentication` / `usage_limit`が壁（`JobFailure::wall`）になる。壁は`domain::queue_hold::Wall`（`Authentication` / `UsageLimit`）で、askの理由と`subject`、記録するevent（`auth_required` / `usage_limited`）、questionの文面（`AUTH_QUESTION` / `USAGE_LIMIT_QUESTION`）を持つ。askは`NewHold::wall`で作る（optionsは`done` / `cancel_affected`、`asked_by: supervisor`）。

- **ログインの切れ**（`auth_required`）: 行（枠と`⎿` / `⏺`を除く）が`API Error: 401` / `Invalid API key` / `OAuth token has expired` / `OAuth token revoked`で始まり`/login`を含む
- **利用上限**（`usage_limited`）: 行が`Claude AI usage limit reached` / `Claude usage limit reached` / `You've hit your limit` / `You've hit your usage limit` / `You've reached your usage limit` / `Credit balance is too low`で始まるか、`<24文字以内の窓の名前> limit reached ∙ resets ...`（`·`も）の形（`5-hour limit reached ∙ resets 3pm`、`Opus weekly limit reached · resets Mon 9am`）
- **worker の turn**: 非対話の turn の authentication / usage_limit を provider_wall が読む。使えるもう一方の provider があれば次の turn から切り替え、使えなければ hold の ask で待つ。worker の画面からの検知は廃止した。
- **headless jobの出力**: 失敗したjob（exit非0・時間切れ・読めないverdict。verdictが読めて適用に失敗したものは読まない）のstdoutとstderr（Claudeのobserverは`output.out`と`output.err`を改行でつなぐ）を起動したproviderが分類する（Codexのobserverは役割がproviderを書いたときだけ`AgentProvider::job_failure`で読み、askには入れず`provider_unusable`にする。task 1223、[Observer](observer.md#codexで動かす)）。Claude Codeでは`AgentSignals::job_failure`（中は`job_wall`）が行ごとに読み、JSONの行は`result` / `error` / `message`の文字列も読む（先頭の`Error: `は除く）。成功したjobの出力は読まない（作業の中で文言を引用しても壁にしない）。見つかればjobをaskに足し（`Supervisor::raise_job_wall`、observerは`observer::hold_wall`）、`auth_required` / `usage_limited`（`job`、`entry`、`error`の末尾、`ask_id`）をjobのrunに、runの無いjob（plan review・goal review・observer）はqueueに書く。askの控えはそのpassから効く（同じpassで後から始まるはずのplan review・goal reviewも始めない）。jobごとの扱い:
  - review: `review_failed`も`approve_landing`のaskも作らず、`Phase::ReviewHeld`でsessionを開いたまま待ち、控えが解けたpassでreviewをやり直す。`affected`に載るのはrunではなくreviewのjobなので、`cancel_affected`でもrunは手放さず、askが閉じた後にreviewをやり直す
  - 終わったrunの復旧job: 従来どおり`triage_failed`を書くが、askに居る間は`triage by hand`のattentionにしない。`done`が`job_restarted`で起動し直す
  - 生きているsessionの復旧job: そのalertのask（ADR-t609-1）を開かず、失敗も記録しない。控えが解けた後のpassでalertの次のjobが始まる
  - plan review・goal review: 従来どおり`plan_review_failed` / `goal_review_failed`を書くが、askに居る間は`plan review by hand` / `goal review by hand`のattentionにしない。`done`が出し直す・rearmする
  - observer: `observe_finished`に`wall`と`hold_ask_id`を足す。控えが解けると次の間隔で動く
- **affectedのjob**: askの`affected`にはrunのIDと並べてjobの項目（`domain::queue_hold::HoldJob::entry`: `review job of run <id>`、`recovery job of run <id>`、`plan_review job of proposal <id>`、`goal_review job of goal <id>`、`observer job`、`throughput_review job`（Claudeを書いたスループットの見直しが`[provider_fallback] jobs = false`でClaudeの壁に当たったとき。task 1858））を足す。jobの項目は空白を含み、runのIDは含まないので区別できる（`is_run_entry`・`affected_runs`）。questionの末尾は`Affected: run <id>, review job of run <id>, ...`（古いバイナリの`Affected runs:`も書き直せる。`NewHold::base_question`）。runの無いjobが足されたときの`ask_updated`はqueueイベント（taskもrunも無い）になる。attentionは`health::attention`が、閉じていない対象のaskに居るjob（`queue_hold::job_held`）の失敗を出さず、ask自体をattentionにする

## providerごとの控え（ADR-t813-2）

[ADR-t813-2](../../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)の決定6（ADR-0047決定42をamends、task 818）で、控えの単位はproviderごとになった。このページの`queue_hold`のaskはClaudeの控えで、Codexの控えはaskを開かないqueueイベント（`provider_held` / `provider_released`、[provider-lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)）。

- **workerのclaimとturn**: 使えるproviderがあれば止めない。askが開いていても（Claudeが控えられていても）Codexのworkerが動かせれば、新しいclaimは控えず（`claim_held`を書かない）、taskは非対話のCodexで始まる。Codexが控えられていればCodexのtaskは非対話のClaudeで始まる。両方が使えない（Claudeのaskが開き、Codexが無いか控えられている）ときだけ、下の「控え」のとおりaskが新しいclaimを止め、候補は`claim_deferred`（`provider_unavailable`）で控える。`[provider_fallback] workers = false`のときは、Claudeのtaskはもう一方へ移らずClaudeの控えが解けるのを待ち（`provider_fallback_off`で控え、`--no-claude`の下を除く）、Codexのtaskは今どおり動くのでaskは新しいclaimを止めない（[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)の「切り替えを止める設定」、[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）
- **askを開くとき**: Claudeが止まったとき（headlessのjobの出力、Claudeの非対話のturn、非対話のruntimeのplannerのturn）は、Codexが使えても開く（Claudeで動く役割のjob（`provider`を書かない復旧・review・goal review・plan review・observer）は人を待つ）。Claudeの非対話のturnからCodexへ移ったrunはaskの`affected`に入れず、runに`auth_required` / `usage_limited`（`switched_to: codex`）を書く。非対話のruntimeのplannerのturnがClaudeのログイン切れ・利用上限で止まったときは、runもjobも名指さないaskを開くか開いているaskに加わり（`affected`に入らない）、queueのevent `provider_waiting`（`planner_id`・`turn`・`reason`・`ask_id`）を残す。plannerは終わらずに待ち、答えで控えが解けた後のpassで、失敗したturnの依頼を載せた`provider retry`のturnで続ける（`tend_planner_walls`、[`plan` / `planners`](plan-planners.md#runtimeのplannerの経路)の「使えないとき」、ADR-t1394-2決定5）。Codexだけが止まったときは開かない。止まったrunがもう一方へも移れないときはrunを失敗にせず待たせ（`provider_waiting`）、Claudeの認証と利用上限か、両方使えないときは、providerに関わらずrunが`raise_wall`でaskに入る（壁は開いている控えのaskの理由か、認証・利用上限の側の理由）。切り替えの上限でCodexから移れずClaudeが使えるときはaskを開かず、Codexの控えが解けた後に同じthreadへもう一度送る
- **`done`**: 下の適用に加え、askを閉じるsupervisorがCodexの控えも解く（`provider_released`の`why: done`。どのsupervisorも毎passでqueueの控えを読み直す）。控えのaskに入って待つrunへの「続けて」は、runの今のproviderに行く
- **Claudeで動く役割**: `[roles.recovery]`に`provider`を書かない復旧と、`[roles.observer]`に`provider`を書かないobserver、`[roles.review]`に`provider`を書かないrunのreview、`[roles.goal_review]`に`provider`を書かないgoal review、`[roles.plan_review]`に`provider`を書かないplan review、`[roles.throughput_review]`に`provider`を書かないスループットの見直し（askのあいだ始めない）の控えは今までどおりaskに従う。`provider = "codex"`のobserverはClaudeの控えを待たずCodexで動き、Codexの失敗はaskに入れない（task 1223、[Observer](observer.md#codexで動かす)）。`provider = "codex"`の復旧jobも同じくClaudeの控えの間もCodexで始まり、Codexの失敗はaskに入れない。Codexが使えずに止まったjobはCodexを控えて次のjobをもう一方で（`[provider_fallback] jobs = false`なら控えが解けた後にCodexで）始め、それ以外の失敗はjobの今の失敗の経路（終わったrunは`triage_failed`、生きているrunはalertのask）に入る（task 1225、[Triage](triage.md#codexで動かす)）
- **providerを書いた役割のjob**（[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)の決定5がADR-t813-2の決定6をamends、task 1065。今はgoal review・plan review（task 1218。[Plan review](plan-review.md#codexで動かす)）・スループットの見直し（task 1220。[Throughput review](throughput-review.md#codexで動かす)。Codexが止まると終わりの`provider_unusable`を見てsupervisorが`ProviderHold`で控え、その期間をもう一方で始め直す）・observer（task 1223。[Observer](observer.md#codexで動かす)。スループットの見直しと同じく、Codexが止まると終わりの`provider_unusable`を見て控え、そのobservationをもう一方で起動し直す）・復旧job（task 1225。Codexが止まると控え、`provider_unusable`を書いたそのjobの終わりは人のものにせず、終わったrunは次のラウンド、生きているrunはalertの次のjobをもう一方で（`[provider_fallback] jobs = false`なら控えが解けた後に同じproviderで）始める）とrunのreview。runのreviewは[ADR-t1207-1](../../adr/2026-09-30-t1207-1-codex-run-review.md)）: jobを止めるのは、その役割のproviderと切り替え先（その役割を動かす実装を持つもう一方のprovider）の両方が控えられているか、supervisorに無いときだけ。Claudeの控えのaskが開いていても、Codexで動けるgoal reviewはCodexで起動し、Codexが控えられていればClaudeで起動する（[Goal review](goal-review.md)の3）。Codexのgoal reviewが認証・利用上限・起動の失敗で止まったときはaskを開かず`ProviderHold`で控え（`provider_held`の`run_id`はnull）、Claudeのgoal reviewが止まったときは今までどおりaskを開いてjobを`affected`に足す。どちらもgoal reviewの行は`interrupted`で閉じ、次のpassでもう一方のproviderで起動し直す（両方控えられていれば起動しない）。runのreviewも同じく、Codexが止まればaskを開かず`ProviderHold`で控え、`review_retried`を記録して同じpassでもう一方で起動し直し、両方使えなければ`Phase::ReviewHeld`で待つ。`--no-claude`では切り替え先が無いので待たず、`review_failed`としてleaseを放し`approve_landing`のaskで人を待つ（ADR-t1207-1 決定 4）。`[provider_fallback] jobs = false`（[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)、task 1858）のときは、これらのjobはもう一方で起動せず、実際に失敗したproviderの控え（Codexの`ProviderHold`、Claudeの控えのaskか起動の失敗の`ProviderHold`）が解けるのを待って同じproviderで起動する。控えと人への届け方は変わらない（[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)の「切り替えを止める設定」）。Claudeの控えでaskを開く条件（Claudeでしか動けない役割があること）は変えない。runtimeのplanner（と`provider = "codex"`を書かない復旧・review・plan review・observer）はClaudeで動くので、Claudeの止まりは今までどおりaskを開く

## どのaskが控えるか

`domain::queue_hold::reason_of`: kindが`queue_hold`で、理由が`authentication`なら`HoldReason::Authentication`、`cost`で`subject`が`usage_limit`（`USAGE_LIMIT_SUBJECT`）なら`HoldReason::UsageLimit`。`subject`が`disk`のものは対象外で、それ以外の`subject`の`cost`は利用上限と同じに扱う。`hold_of`はそのうちopen（未回答）のものだけを控えにする。

## 控え

supervisorは毎pass（drainの途中も）、ディスクの確認（`check_disk`）の次に`Supervisor::check_queue_hold`でqueueのcloseされていない対象のaskを読み、回答済みのものを適用してから、openなものの最初の1件を`queue_hold`（`claim_hold::QueueHold`: 理由、`ask_id`、`affected`の数）として持つ。持っている間:

- **claim**: `ClaimHold::judge`は`HoldInputs.queue_hold`（どのworkerにも経路が無いときだけ渡す。上の「providerごとの控え」）をディスクとloadより先に判定し、`claim_held`（`reason`が`authentication` / `usage_limit`、`value`がaskの`affected`の数、`threshold`が0、`ask_id`、`message`）を記録してclaimしない。askが閉じるか回答されると次のpassで`claim_resumed`になる（記録の規則と`status`の`claim_hold`・`stats`の`claim_holds.by_reason`は[claimを控える](claim-hold.md)と同じ）
- **review**: 受理されたrunのreviewは`Phase::ReviewHeld`で待ち、sessionは開いたまま、`review_started`を書かない（`start_review` / `retry_review`が入口で`review_route`に聞く。`provider`を書いたreviewが待つのは、そのproviderと切り替え先の両方が使えないときだけ）。控えが解けたpassで`start_review`が始める。引き継ぎとadoptは`awaiting_integration`のreviewの無いrunを`start_review`で組み立て直すので、同じく待つ
- **復旧job**: `[roles.recovery]`に`provider`を書かない復旧jobは、終わったrunのtriage（行き先の`Supervisor::recovery_route`が待つので`triage_candidates`が候補を出さない）を始めない。生きているsessionのalertの復旧job（`RecoveryWatch::start`）も始めず、alertは控えが解けた後のpassでまた拾う。`provider = "codex"`の復旧jobは上の「providerを書いた役割のjob」のとおり控えの間もCodexで始まる（task 1225）（長く走るbackgroundのalertは、`seen`を進める前に判定するので失われない）
- **plan review・goal review・observer**: 起動しない（`plan_review_pass`に`starting: false`。goal reviewは`goal_review_pass`に控えによらず`starting`を渡し、`Supervisor::goal_review_route`が`provider`を書かない役割を控えの間は起動しない。observerはqueue serviceが居れば控えによらず`start_observer_when_due`を呼び、行き先の`Supervisor::job_start_route`が`provider`を書かない役割を控えの間は起動しない。task 1223）。走っているjobは最後まで追う。`[roles.goal_review]`・`[roles.observer]`に`provider`を書いたgoal review・observerは、上の「providerを書いた役割のjob」のとおり控えの間も使えるproviderで起動する（`provider = "codex"`のobserverはClaudeの控えの間もCodexで起動する）
- 走っているrunはleaseとsessionを持ったまま進む。着地（`integrate`）はClaudeを使わないので控えない

`queue_hold`のaskに入った（`affected`に居る）runの促しとstalledのaskを止める扱い（`hold_of`）と、待ちから`queue_hold`で戻す扱い（[人の答えを待つrun](waiting.md)）はtask 361のまま。促しを止めるのはaskが閉じるまで（`hold_unclosed`。answerの後、supervisorが`done`を適用して続けてよいという文を打つまでの間も含む。task 729）で、その間に促しが先に打たれることはない。

## answerの適用

answerがaskのoptionsのどれか（`queue_hold::applies`）なら、`answer`は`ask_answered`に`runtime_delivers: true`を書き、`status`はattentionを`applying the answer of ask N (runtime)`（`ApplyingAnswer`）として人の操作を求めない。ディスクのaskの`done` / `wait`も同じ（supervisorが`apply_disk_answers`で適用する）。optionsに無い自由な答えは`runtime_delivers: false`で、inboxが人に見せる（`read the answer of ask N and close it`）。回答済みのaskは控えない。

答えは、runを持つsupervisorがそれぞれ自分のslotのrunに適用する（task 754）。どのsupervisorも毎passで、回答済みで閉じていない対象のaskを読み（`Supervisor::apply_hold_answer`）、`affected`のrun（jobの項目は除く）のうち自分のslotに居て、まだそのaskの`hold_answer_applied`が無いものに答えを適用し、runに`hold_answer_applied`（`ask_id`、`answer`、`outcome`: `continued` / `released` / `moved_on`、`supervisor`）を書く。印は答えが働く前に書く（手放したrunはleaseを持たないので、印より先に手放すと、同時にaskを閉じる別のsupervisorが`unwatched`と読んでしまう）。この印が1つのrunに答えを1回だけ適用させる（次のpassも、runを引き継いだ・adoptした別のsupervisorも、印のあるrunには適用しない）ので、`done`の文面の二重送信や二重のabandonは無い。

- **`done`**（`hold_outcome`が`continued`）: 生きているsessionを見ている段（`Phase::Session` / `Revise` / `Resume`、`Phase::holds_live_session`）に居るrunを`hold_continue`に入れ（`continued`）、そのsessionの監視（`SessionWatch::continue_after_hold`。workerのsessionではwrapperが生きていて終了の依頼の前、receiptが無いとき、reviseとresumeでは答えの配送と同じ位置で、送った後はその段の時間切れを数え直す）が決まった文面`queue_hold::CONTINUE_TEXT`を1回送る。送信は`submit`が次のturnの依頼としてrun dirの`turns/`に書く（入力欄の確認・Enterの送り直し・`StartCheck`による取り込みの確認（ADR-0047の決定31）は、対話のworkerとともにtask 1437で撤去した）。送ったらrunに`hold_continue_sent`（`ask_id`、`workspace_id`、`submitted`）を書き、stallの計時は送った時刻からやり直す。送れなかったらlogだけ残し、stallの促しが続きを頼む。receiptの後やreview・exitの段に進んだrunには送らない（`moved_on`）
- **`cancel_affected`**（`hold_outcome`が`released`、`Supervisor::hold_canceled`）: 生きているsessionの段（`Session` / `Revise` / `Resume`）かreviewの待ち（`ReviewHeld`）に居るrunを、slotから外してheadless jobを止め、`abandon`と同じく手放す（`released`）: `runtime_error`（`message`、コード`hold_canceled`、`lease_released: true`）を書いてleaseを返す。statusは変えず、sessionとworktreeは残る。inboxには`recover run`のattentionとして出る（成果を捨てるかどうかは、この答えを選んだ人の判断で、`recover`の手順で決める）。自分で先へ進んだrun（validation・review・着地の途中）はもう止まっていないので手放さない（`moved_on`）

### askを閉じる

適用の後、そのsupervisorはaskを閉じられるかを見る（`Supervisor::close_hold`）。`affected`のrunがどれも次のどれかなら閉じる:

- `hold_answer_applied`がある（どのsupervisorが適用したかは印の`supervisor`）
- leaseが無い（`unwatched`: 終わったか手放されたrunで、答えを適用するsessionが無い）
- 答えから300秒（`APPLY_WAIT_SECS`）が過ぎた。そのとき別の生きているsupervisorがleaseを持つのに適用していないrunは`elsewhere`（そのsupervisorが答えを読めない古いバイナリか、passが止まっている）、leaseがstaleなrunと、このsupervisorのleaseなのにslotに無いrunは`unwatched`になる

runのeventかleaseが読めなかったpassは閉じず、次のpassで読み直す。引き継ぎ（exec）の後のsupervisorは、runのslotを組み立て直すまで答えを適用しない（組み立て直す前のpassは控えを読むだけ）

別の生きているsupervisorがleaseを持つrunが適用されていない間は閉じず、そのsupervisorの次のpassが適用する。leaseがstaleなrunも300秒までは待つ（生きているsupervisorがadoptすれば、自分のslotのrunとして適用する）。閉じていない間は`hold_unclosed`がそのrunの促しとstalledのaskを止めたままなので、`done`の文面より先に促しが打たれることはない。回答済みのaskは控えないので、claimとheadless jobは答えた時点で再開する。

閉じたsupervisorだけが（`close_ask`に負けたsupervisorは何もしない）、`done`なら失敗したjobを起動し直し、queueに`queue_hold_applied`（`ask_id`、`answer`、`reason_category`、`subject`、`continued`、`released`、`moved_on`、`elsewhere`、`unwatched`（runのIDの配列。どのrunもこの5つのどれか1つにだけ載る）、`runs`（runごとの`{run_id, outcome, supervisor}`。`supervisor`は適用したsupervisorか、`elsewhere` / `unwatched`ではleaseのtoken、leaseが無ければnull）、`restarted`、`jobs`（askの`affected`のjobの項目）、`supervisor`（閉じたsupervisor））を書く。`done`で起動し直すのは、askが開いた時刻の600秒前（`FAILED_BEFORE_ASK_SECS`。askはsessionかjobがエラーを見せてから開くので、その直前に記録された失敗を含める）以後に失敗したjob:

- 終わったrunの復旧job（`triage_failed`）: runが`failed` / `interrupted`でleaseが無く、triageの状態が`Failed`でその失敗が最新なら、runに`job_restarted`（`job: triage`、`ask_id`、失敗の`event_id`）を書く。`triage_state`は`job_restarted`（`job: triage`）を`Pending`に戻すので、次のpassの`triage_candidates`が拾う（試行回数は数え続け、上限を超えれば従来どおりaskになる）
- plan review（`plan_review_failed`）: proposalがまだ失敗で止まっていれば、`submit --proposal ID`と同じく出し直す（`proposal_resubmitted`）
- goal review（`goal_review_failed`）: goalがまだ失敗で止まっていれば、`goal review ID`と同じくrearmする（`goal_review_rearmed`）
- 起動し直さないもの: 壁で止まったreviewは`ReviewHeld`で待っているので、控えが解けたpassで自分でやり直す。それ以外のrunのreviewの失敗は失敗のときに`approve_landing`のaskを開いている（task 328）ので人の答えに任せる。生きているsessionの復旧jobの失敗はそのalertのask（ADR-t609-1）になり人の答えに任せ、askはsessionが進めば閉じる（壁で止まったものはaskにせず、控えが解けた後のpassで次のjobが始まる）。observerは次の間隔で動く

`cancel_affected`はjobを起動し直さず、askが閉じるので、失敗したjobは従来のattention（`triage by hand`など）に戻る。`ReviewHeld`で待つreviewは控えが解けたpassでやり直す。`elsewhere` / `unwatched`に載ったrunには答えが適用されていないので、`cancel_affected`ならinboxが`queue_hold_applied`を見て`recover`で手放し、`done`ならそのrunのstallの促しが続きを頼む。task 754より前のバイナリのsupervisorは、答えを読むと自分のslotにだけ適用してすぐaskを閉じる（他のsupervisorのrunは`elsewhere`）。引き継ぎ（exec）の後のsupervisorは、runを組み立て直す前に控えを読むので、待っていたreviewを始めない。

## 記録とtest

- queueイベント: `claim_held` / `claim_resumed`（理由`authentication` / `usage_limit`、`ask_id`）、`queue_hold_applied`、runの無いjobの`ask_updated`・`auth_required`・`usage_limited`（`EventKind::is_queue`）
- runイベント: `hold_answer_applied`、`hold_continue_sent`、`job_restarted`、`runtime_error`（`hold_canceled`）、`auth_required` / `usage_limited`（画面のものと、`job`を持つjobのもの、Codexへ移ったrunの`switched_to`を持つもの）、`provider_switched`
- queueイベント（Codexの控え）: `provider_held`、`provider_released`
- queueイベント（非対話のruntimeのplanner）: `provider_waiting`（`planner_id`・`turn`・`provider`・`reason`・`message`・`ask_id`。runの`provider_waiting`はrunイベント）
- test: `tests/it/runtime_queue_hold.rs`（利用上限のaskの控えでclaimとreviewが待ち、`done`で再開する／`cancel_affected`でrunを手放す／`done`で失敗したtriageを起動し直す）、`tests/it/runtime_queue_hold_shared.rs`（2つのsupervisorが1件のaskのrunを1つずつ持ち、`cancel_affected`で両方が手放され、`done`でどちらのsessionにも文面が1回ずつ届く／別の生きているsupervisorがleaseを持つrunを待ってから閉じ、leaseの無いrunを`unwatched`にする）、`application::supervise::stall`のunit test（holdの間のidleは促さない）、`tests/it/runtime_queue_hold_detect.rs`（利用上限の画面で`cost`のaskに入る／2つのreviewがログインの切れで1件のaskにjobとしてまとまり、`done`でやり直して着地する／runとjobが1件のaskに並ぶ）、`tests/it/plan_review.rs`の`a_plan_review_at_the_usage_limit_joins_the_cost_ask_and_starts_again_after_done`、`tests/it/planner_headless_turns.rs`の`a_request_written_during_a_turn_at_the_usage_limit_waits_for_the_retry_of_that_turn`（非対話のplannerのanswerのturnが利用上限で`cost`のaskを開き、`provider_waiting`で待ち、`done`の後に同じanswerを載せた`provider retry`のturnで続ける）と`application::supervise::planner_turns`のunit test（ログインの切れは`authentication`のask、利用上限は`cost`のask、起動の失敗はClaudeの控え。task 1711）、`infrastructure::claude`（`auth_required`・`usage_limited`・`job_wall`）・`domain::queue_hold`・`domain::claim_hold`のunit test
