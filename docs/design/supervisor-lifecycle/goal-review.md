---
id: design-supervisor-lifecycle-goal-review
type: design
title: "Goal review (supervisor)"
status: current
created: 2026-09-27
scope: runtime
related:
  - adr-t1504-2
  - design-follow-up-membership
  - adr-t1566-1
  - design-supervisor-lifecycle-prompt
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-plan-review
  - design-supervisor-lifecycle-draft-planners
  - design-supervisor-lifecycle-actor-model
  - design-provider-lifecycle
  - adr-0047
  - adr-t1063-1
---

# Goal review (supervisor)

所属taskがすべて終わったopenのgoalを、supervisorが起動するheadlessのgoal review jobがacceptanceと照合し、そのverdictと人のanswerをruntimeが適用する（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定43）。
use caseは`src/application/supervise/planning/goal_review.rs`、storeは`src/infrastructure/goal_reviews.rs`（port `GoalReviewStore`）、verdict・answer・起動の条件の型は`src/domain/goal_review.rs`、promptは`prompt::goal_review_prompt`。
plan reviewと同じくrun slotは使わず、loopの各passで進める（`goal_review_pass`）。
`--once`のloopは走っているgoal reviewを待つ。
drain中（`stop`、provisioningの失敗）は新しいgoal reviewを始めない（走っているjobとanswerの適用は続ける）。
execの[引き継ぎ](handoff.md)のあいだの回収と止め方は引き継ぎの文書が持つ。
止めたjobの未完了の行は、引き継いだsupervisorが最初のpassに入る前に、候補が無くても自分のtokenの分だけ`interrupted`で閉じ、他のtokenの行は触らない（queueが取らなかった終わり（verdictか失敗）を持っていれば、その行は閉じずにそれで終える。[Supervise](supervise.md#queue-serviceの経路)）。
閉じるときはtranscriptをtransactionの前に`read_before`で読み、既存のclose経路で区間の`session_closed`を記録する。
候補が残れば以後のpassでreviewをやり直し、`interrupted`はattemptに数えない。

1. **記録**: jobは`goal_reviews`の1行。
   互換のmigrationは外部キーとunique indexを持てないので、同時に1つは`begin_goal_review`が`BEGIN IMMEDIATE`の中で生きているsupervisorの未完了の行が無いことを確かめてから行を書くことで守る（queue全体で1つ。plan reviewとは別に数える）。
   生きているsupervisorは、登録があり、heartbeatが`HEARTBEAT_TIMEOUT_SECS`より新しいか、古くてもpidが生きているもの（`domain::job_supervisor_lives`。sleepから戻った直後のhostでは全員のheartbeatが古いので、pidで見て待つ。自分と同じpidの登録（in-processのtest）はpidで生きているとしない。[headless jobの引き継ぎ](headless-job-processes.md#引き継ぎ)と同じ規則）。
   生きていないsupervisorの未完了の行と、自分のtokenの未完了の行（execの前の自分の行）は`interrupted`で閉じる（queueが取らなかった終わりが残る行は、その前に引き取ってそれで終える。[Supervise](supervise.md#queue-serviceの経路)）。
   eventはgoalに記録する（`run_events.goal_id`）。
   jobのpromptと出力はqueueの`goal-reviews/<行のID>/`に置く。
2. **起動の条件**（`goal_review_candidates`、ID順の先頭を取る）: goalが`open`で閉じておらず、所属taskが1件以上あり、すべて`completed`か`canceled`か`replaced`で所属taskの`completed`が1件以上（`goal_review::tasks_done`。`draft` / `submitted`のtask、つまりruntimeのplannerの待ち・`keep_draft`で残ったdraft・閉じていない`planner_question`の対象は、これで除かれる）、元goalがこのgoalのfollow_up（所属は問わない）に所属の判断が済まないもの（[Follow-up membership judgements](../follow-up-membership.md)の「閉じる条件」、ADR-t1504-2決定8）が無く、goalのreviewが開いた`approve_goal`のaskで閉じていないものが無く、前回のgoal review（`interrupted`を除く最新の行）の後に入力が変わっている。
   `replaced`は終わったものに数えるが`completed`には数えず、置換先を再置換も辿った全てのtaskが所属を問わず終わるまで起動を待つ（[ADR-t1521-2](../../adr/2026-10-05-t1521-2-trusted-runtime-parks-snapshots-and-atomically-replaces-tasks.md)決定5）。
   置換先の記録が空か、存在しないtaskを指すか、辿ると循環する置換元（再置換で辿った置換元を含む）は解決済みとせず（`replan_apply::unresolved_replacements`）、そのgoalは起動せず、achievedの適用と`goal close --verdict achieved`も拒み、定期taskのkeyも保つ。
   その記録は手で直さないので、`status`（inbox）のattentionに`replacement_unresolved`（`goal review by hand`）としてその置換元のtaskで出て、作業を足しても起動しないので、人がgoalを`abandoned`で閉じる（続ける作業は別のgoalに足す）。
   out_of_scopeと判断して別のgoalへ移したfollow_upの未完了は起動を妨げない。
   follow_upの所属の判断だけで起動しないgoalのattention（`goal_follow_ups_unsettled`）は[Follow-up membership judgements](../follow-up-membership.md)の「閉じる条件」が持つ。
   入力は`goal_review::review_fingerprint`で比べ、同じなら起動しない。
   follow_upの登録、判断の記録・訂正、acceptanceの変更は入力を変える。
   置換の系統（置換元・置換先・条件の対応・計画と条件の版）も入力に入り（`goal_review::with_replacements`）、jobには系統と、別のgoalへ移った置換先のtaskも渡す（[ADR-t1521-2](../../adr/2026-10-05-t1521-2-trusted-runtime-parks-snapshots-and-atomically-replaces-tasks.md)決定5）。
   goalの外のfollow_upのstatusは入れない（範囲外の作業が進んでもreviewを無効にしない。終わったかは閉じる条件が見る）。
   人の`goal review ID`（`rearm_goal_review`）は最新の行に`rearmed_at`を書き、状態が同じでも起動させる。
   openでないgoalには打てない（`goal_review::rearmable`）。
   取るかどうかは副作用の無い`goal_review::reviewable`、失敗の後に人を待つかは`goal_review::waits_for_a_person`が決め、storeは行から読んだ値を渡す。
3. **起動**（`start_goal_review`）: `[roles.goal_review]`から起動の`launch`を決め（[Actor model](actor-model.md)。表が無ければmodel / effortを渡さない）、`goal_review_started`を書き、jobをactor executor（[Roles](roles.md#actorの起動actorexecutor)）の`HeadlessProgram::Job`（権限の意図`GOAL_REVIEW_ACCESS`。[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）で起動する。
   `session_id`は`begin_goal_review`が行と同じトランザクションで作るUUIDで、jobにそのまま渡す（ADR-0048の決定4）。
   jobの区間（kind `goal_review`）は`goal_review_started`で開き、同じ`goal_review_id`の`goal_review_finished` / `goal_review_failed`で閉じ、行を`interrupted`で閉じたときにも閉じる。
   区間のeventはgoalの最初のtask（approve_goalのaskと同じ）に記録し、`stats --goal`で絞り込めるようにgoalのIDを持つ（[Agent provider lifecycle](../provider-lifecycle.md#claude-sessionの区間)）。
   envは`actor_env`が組み、`DAGQ_QUEUE`は渡さず、jobの`dagq`はクライアントモードでserviceに送る（[Queue service](../queue-service.md#クライアントモード)。CLIは読み取りだけを許す。[Roles](roles.md#actors)）。
   時間の上限はreviewの`review_timeout`。
   起動できなければ下の6の失敗にする。
   - **provider**（[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)）: 行き先は`PlanningState::goal_review_route`が`starting`のpassごとに、supervisorの状態を値で渡した副作用の無い`goal_review_route_of`で決める。
     `[roles.goal_review]`に`provider`が無ければClaudeで、Claudeの控えのaskが開いている間は起動しない。
     `provider`があれば`domain::actor_model::job_route`で、そのproviderが使えれば（supervisorにagentがあり、`Supervisor::provider_held`が控えを言わない）そこで、使えなければもう一方のprovider（goal reviewはClaudeでもCodexでも動く）が使えればそこで（modelとeffortは渡さない）、どちらも使えなければ起動しない。
     切り替えを止める設定（[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）のときは、使えない理由ではもう一方で起動せず、控えが解けるのを待って同じproviderで起動する（`--no-claude`ではClaudeを書いた役割もCodexで起動する。[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)の「切り替えを止める設定」）。
     Codexのjobは`Ports::codex_jobs`（supervisorが見つけて`--version`の通ったCodex）で組み、promptはstdinで渡し、executorがread-onlyのsandboxをqueue serviceのsocketだけを許すpermission profileに置き換えて起動する（[Agent provider lifecycle](../provider-lifecycle.md#codexのheadless-job)、[Queue service](../queue-service.md#codexのsandboxからの到達adr-t1233-5決定4)）。
     sandboxの中の`dagq`も同じroleで読むだけ。
     Codexのjobは`session_id`を受け取らないので、`goal_review_started`の`session_id`はnull。
     `headless_jobs`の行のproviderも行き先のprovider。
4. **入力**（`goal_review_material`）: goalの欄、goalのfollow_upとその判断の履歴、所属taskとその着地のreceipt、goalのevent、前回までのgoal review、`gaps`が続いた回数。
   jobはmain checkoutの文書とsource、読み取りの`dagq`で残りを読む。
   promptの渡し方・節ごとと全体の上限・選ぶ順・省いたことの明示・byte数の記録は[Prompt](prompt.md#headlessのjobのprompt)の「headlessのjobのprompt」と[上限の節](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)が持つ（[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)。ADR-0047決定43の入力の列挙をamendsした）。
   promptは、out_of_scopeと判断したfollow_upを達成の判定に含めず（その作業を待たず、gapにもしない）、requiredと判断したものは所属taskと同じく判定に含めるよう指示する（ADR-t1504-2）。
5. **verdict**（`GoalReviewVerdict`、stdoutの最後のJSON object）: runtimeは`goal_review::decide`で、同じgoalで直近から続いた`gaps`の数（storeが行のoutcomeを新しい順に読み、`goal_review::gaps_in_a_row`が数える）が`MAX_GOAL_GAPS`以上の`gaps`を`ask`に変える。
   適用（`finish_goal_review`）は1トランザクションで、jobの行がまだ自分のもので、goalがopenのままで、2の入力が起動時と同じで、まだ2の条件（所属taskがすべて終わり、follow_upの所属の判断が済んでいる）を満たすことを再検査し、崩れていれば`interrupted`で閉じて何もしない（`interrupted`は前回に数えないので、条件がそろえば取り直す）。
   reviewの間のfollow_upの登録、判断の記録・訂正、acceptanceの変更の後のverdictはこれで使われない。
   検査はtransactionの中のqueueの状態だけで行うので、execの引き継ぎで回収したverdictも、supervisorの再起動の後も、同じ`finish_goal_review`で同じ結果になる。
   - **achieved**: `goal close --verdict achieved`と同じ検査でgoalを閉じる（`close_goal_in`）。
   - **gaps**: 各gapを、そのgoalの`draft`のtaskとして登録する（`register_gaps`）。
     `draft_origins`に出どころ`goal_gap`を書くので、以後は[Draft planners](draft-planners.md)の経路でruntimeのplannerが採否を決める。
     goalは開けたまま。
     draftがある間は2の条件を満たさず、draftが終われば（採用されて着地する、cancelされる）状態が変わって再び起動する。
   - **ask**: inbox宛ての`approve_goal`のaskを、goalの最初のtask（IDの最小）について作る（runは無い）。
     `reason_category`はverdictの`discard`ならそれ、ほかは`scope`。
     optionsは`GOAL_OPTIONS`にverdictの`options`を重ならないように足す。
     askのIDを行の`ask_id`に書く。
     作ったaskは誰にも通知を送らず、inboxは`watch --role inbox`の`ask_opened`で受ける。
   - どのverdictも`goal_review_finished`を記録する。
     Codexのjobは加えてthreadから読んだsessionとmodelを持つ（`AgentProvider::job_session`）。
     verdictはproviderによらず、providerの実装が取り出した最終の返答のtextを同じ`GoalReviewVerdict::parse`と同じ`finish_goal_review`の再検査で適用する。
6. **失敗**（非0の終了、時間切れ、JSONが無い・形が合わない、起動できない）: goalを動かさず、行を`failed`にして`goal_review_failed`を記録する。
   2の入力が同じなので、2の入力が変わる（所属taskの状態の変化、follow_upの登録、所属の判断の記録・訂正、acceptanceの変更）か人が`goal review ID`を打つまで起動しない。
   そのあいだ`goal_review_holds`がそのgoalを返し、`status`（inbox）のattentionに`goal review by hand`としてgoalの最初のtaskで出る。
   人はgoalを`goal close`で閉じるか、plannerでtaskを足すか、`goal review ID`で取り直させる。
   - **使えないprovider**（ADR-t1063-1の決定4・5）: `[roles.goal_review]`に`provider`を書いたときだけ。
     jobの起動の失敗か、verdictの無い失敗のjobの出力の分類（`Supervisor::job_failure`）がproviderを使えないと言えば、Claudeの認証・利用上限は控えのaskを開いてjobを`affected`に足し、それ以外（Codexのどれも、Claudeの起動の失敗）は`hold_provider`で`ProviderHold`に控える。
     引数の上限とstdinを用意できない失敗はproviderを控えず、そのjobの一般の失敗にする（[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）。
     その上で`goal_review_failed`に`provider_unusable`を足し、行を`failed`でなく`interrupted`で閉じる。
     `interrupted`の行は2の「前回のgoal review」に数えないので、goalはすぐ候補に戻り、次のpassで3の行き先（もう一方のprovider、両方控えられていれば起動しない。切り替えを止める設定なら控えが解けるまで起動せず、解けた後に同じprovider）で起動し直す（新しい行、同じ`attempt`）。
     切り替えを止めても失敗の分類・控え（控えるのは失敗したproviderだけ）・Claudeの控えのaskは同じで、`job_failure_route`は控えるかだけを`provider`を書いたことで決める。
     `goal review by hand`のattentionにもならない（`status`は`goal_review_holds`が`interrupted`の行を返さず、`watch` / `events`は`event_attention`が`provider_unusable`を持つ`goal_review_failed`をattentionにしない）。
     起動の失敗のうちjob自身の用意（directory、promptの組み立てと書き込み）の失敗はproviderの判定に使わず、一般の失敗にする。
     利用上限の控えの解ける時刻は、失敗の文とjobのstdoutから読む。
     一般の失敗（非0の終了、時間切れ、verdictが読めない、modelの誤り）は切り替えず、上のとおり`failed`で人を待つ。
     `provider`を書かないgoal reviewは、Claudeの壁ではaskに足され、`failed`で閉じる（askの`done`がrearmする）。
   - Codexのjobの`goal_review_failed`も、読めればsessionとmodelを持つ。
7. **answer**（`goal_answers` → `decide_goal`、1トランザクション）: `GoalAnswer::parse`で`GOAL_OPTIONS`の答えを読む。
   適用できるのは、goalが閉じておらず、`achieved`は所属taskがすべて終わり、follow_upの所属の判断が済み、置換先の記録が壊れた置換元が無く（2の条件のうちこの3つだけ）、`abandoned`は`in_progress`が無く、`gaps`はreviewのverdictにgapがあるとき（`gaps: <text>`は人のtextを1件のgapにする）。
   適用するとaskを閉じ、`goal_decided`を記録する。
   `achieved` / `abandoned`はgoalを閉じ、`gaps`は5と同じ手順でdraftを登録し、`keep_open`は何もしない（2の入力が同じなので、2の入力（所属task・acceptanceの版・follow_upとその判断）が変わるまで起動しない）。
   goalが先に閉じていたらaskを閉じ、`ask_closed`だけを記録する。
   適用できないanswer（verdictが足した独自のoption、書き足しの自由文、条件の崩れた`achieved`など）は閉じずに残し、inboxの人が読んで`ask close`する。
   `goal_answers`は答えた時点の`ask_answered`が`runtime_delivers: true`のaskだけを返すので、inboxに渡したanswerを後で条件がそろってからruntimeが適用することはない。
   `keep_open`はgoalの行の`rearmed_at`を消す（askの前に打たれた`goal review ID`で直ちに取り直さないため）。
   `ask_answered`の`runtime_delivers`はanswer時に`goal_answer_applies`で決める。
   `status`のattention（`applying the answer of ask N (runtime)`）はsupervisorと同じ`goal_answers`を読み、最新の`ask_answered`の`runtime_delivers`がtrueでaskがcloseされていないときだけ出す。
   現在のgoalの状態で判定し直さない。
8. **plannerの役割**: plannerはgoalの完了を見てcloseする役を持たない（ADR-0047の決定16の最後の項）。
   `goal close`は人の判断（askのanswer、inboxに頼んだ計画の依頼のplanner、人が`DAGQ_ROLE`の無い自分のterminalで打つ`goal close`）のために残る。
9. **achievedの後の訂正**（ADR-t1504-2決定9、`correct_goal`のask）: achievedで閉じたgoalを元goalとするfollow_upに、plannerか人が`judge-follow-up`でrequiredを記録すると（新しい判断か、out_of_scope・undecidedからの訂正。requiredの確かめ直しは問わない（`follow_up::opens_correction`）。記録は[Follow-up membership judgements](../follow-up-membership.md)の「achievedの後の訂正」）、同じトランザクションでinbox宛ての`correct_goal`のaskをそのfollow_upのtaskについて作る（runは無い、`reason_category`は`scope`、`asked_by`は`supervisor`、optionsは`reopen` / `correct_verdict` / `keep_achieved`）。
   inboxは`watch --role inbox`の`ask_opened`で受ける（[通知](ask-push.md)）。
   questionは所属の判断の中身と、goalを待っていて閉じたときに解放されたtaskとそのrunを載せる（`follow_up::correction_question`。runtimeはどれも止めない）。
   goalの閉じた状態とverdict、`goal_closed`と前の判断の行は変えない。
   answer（`correction_answers` → `decide_correction`、1トランザクション）は、goalがまだachievedで閉じていてanswerがoptionのどれかのときだけsupervisorが適用し（`ask_answered`の`runtime_delivers`はanswer時に`correction_answer_applies`で決める。supervisorの`correction_answers`は最新の`ask_answered`の`runtime_delivers`がtrueでaskがcloseされていないものだけを選ぶ。`status`の`applying the answer of ask N (runtime)`もこの選択を読み、現在のgoalの状態で判定し直さない）、askを閉じて`ask_closed`と`goal_correction_decided`をgoalとfollow_upに記録する。
   `reopen`はgoalを開き直し（`goal::reopen`。statusを`open`に、`closed_at`と`verdict`を消す。`goal_closed`は残る）、`draft` / `ready`のfollow_upをgoalに戻す（それより先のstatusのものと、goalを待つ依存があって移せないものは動かさず、閉じる条件の「requiredなのにgoalの外」で閉鎖を待たせる）。
   同じgoalについて閉じていないほかの`correct_goal`のaskと、閉じる前から残った`approve_goal`のaskは、runtimeが閉じる（未回答なら`ask_answered`をruntimeの答えとして書き、回答済みなら`ask_closed`。`asks::close_by_runtime`）。
   閉じたaskは`goal_correction_decided`に載せる（そのfollow_upの所属は判断の記録か`set-goal`で開いたgoalへ移せるようになる）。
   開き直したgoalはまた閉じられ、それを待つtaskのうちまだclaimされていないものは閉じるまで待つ（走っているrunは止めない）。
   `correct_verdict`はgoalを閉じたまま、達成が誤りだった記録（このevent）を足し、修正はplannerが既存の適切なgoalを探すか修正のgoalを作って行う。
   `keep_achieved`はgoalを閉じたまま、人がacceptanceは満たされていたと判断した記録を足し、follow_upは範囲外として扱う（plannerは所属先を名指したout_of_scopeの訂正を記録できる）。
   ほかのanswerは閉じずにinboxの人が読む。
   適用の時にgoalがもうachievedで閉じていなければ、askを閉じて`ask_closed`だけを記録する。
   走っている依存taskの停止と過去のeventの書き換えはどの答えでもしない。

判断は`src/domain/goal_review.rs`のunit test、providerの行き先とjobの失敗の控えは`src/application/supervise/planning/goal_review.rs`のunit test、境界は`tests/it/goal_review.rs`・`tests/it/goal_review_codex.rs`、follow_upの所属の判断と9の訂正は`tests/it/follow_up_membership.rs`が確かめる。
runtimeの他のtestが使う`TestReviewer`（`tests/it/runtime_support`）はgoal reviewを起動させず失敗にするので、goalのtaskを着地させるtestではgoalは開いたまま残る。
