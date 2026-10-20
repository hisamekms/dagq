---
id: design-supervisor-lifecycle-triage
type: design
title: "Triage (supervisor)"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-background-recovery-job
  - adr-0039
  - adr-0047
  - adr-t1818-2
---

# Triage (supervisor)

終わった run の復旧 job は failed / interrupted / resume_exhausted を扱い、生きている worker の stalled と非対話の idle_process は [生きているsessionの復旧job](background-recovery-job.md) が扱う。

1. **自動recover**: fill passごとに、`integrating`以外でlease行の無いactive runについて`doctor`と同じ判定（`run_health`）を行い、`blockers`が空なら`interrupted`にする。
   taskは`ready`にしない（再実行は復旧jobに委ねる。ADR-0003・ADR-0007の「孤児runは自動再実行しない」は維持し、ADR-0012の「wrapperが死んだrunの`recover`は手動」を`recover`の実行だけ自動に改める）。
   staleなleaseを持つrunは、leaseのpidが死んでいて、adoptの対象（wrapperが生きているか終了を記録した`running` / `validating`、`resume_skipped`の後のrun、`awaiting_integration`）でなければ対象にする（supervisorとwrapperが両方死んだrun）。
   leaseのpidが生きている（heartbeatが止まったまま生きているsupervisor）runは対象外。
2. **alert**: `in_progress`のtaskの最新runで`failed` / `interrupted`のもののうち、最後の`resume_started`より後に`triage_finished`も`triage_failed`も無いもの、または最後のラウンドが`wait`でその`recheck_at`を過ぎたもの（`domain::triage_state`が`Pending` / `Waiting`）。
   alertは、最後の`triage_started` / `resume_started`より後に記録された`recovery_requested`があればその`alert`（resumeを使い切ったrunの`resume_exhausted`。[`needs_session`](needs-session.md#needs_session)の6）、無ければrunのstatusで`failed` / `interrupted`。
   resumeの後にもう一度`failed`になったrunはもう一度ラウンドを持つ。
   新鮮なleaseのあるrun（verdictの後の終了を待っているsessionなど）は待つ。
   行き先（下の「Codexで動かす」の`recovery_route`）が待つあいだは、jobを始めるrunを候補にせず、3でjobを起動せずにescalateになるrun（alertの上限に達し、3の例外に当たらない）だけを候補にする（jobを起動しないのでproviderを要らない。`domain::recovery::round_start`）。
   このsupervisorがまだslotで進めているrun（ラウンドの途中でleaseが他のtokenに移ってstaleになったものも）も取らない: そのslotは次のtickでleaseを失ったとして外れてjobを止め（[supervise](supervise.md#provisionと監視)）、その後のpassでだけ他のrunと同じく次のラウンドを始める（同じrunを2つのslotで進めない）。
   active runが上限未満のときだけ始め、jobの間はrun slotを1つ使う。
3. **開始**: 2の候補は、`needs_session`の再開・readyのclaimと1つの列で、taskの効く優先度の降順（同じ優先度では再開 → 復旧job → claim）に空きslotを受け、順が来たものから始める（[supervise](supervise.md#slotを渡す順とclaim)、[ADR-t1850-1](../../adr/2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)）。
   同じrunでそのalertの`recovery_requested`が上限に達していなければ、`begin_triage`が`BEGIN IMMEDIATE`の中で上の条件（runがtaskの最新runであることを含む）を再検査し（staleなleaseは置き換える。slotにあるrunは2で飛ばしてあるので、置き換えるのはslotの外のrunのleaseだけ）、runにleaseを取って`lease_acquired`、`recovery_requested`と`triage_started`を記録する。
   同じrunを2つのsupervisorが取ろうとしても1つしか通らない。
   runのdirectory（無ければ`runs/<run-id>/`）に`recovery-<alert>-<attempt>.prompt.txt`を書いて、run eventの`recovery_prompt_written`を記録する。
   `triage_started`の`launch`のproviderのheadlessのコマンドをそのdirectoryで、`DAGQ_ROLE=recovery-job`（CLIはreviewと同じく読み取りだけを許す。[Roles](roles.md#actors)）と、queue serviceのsocketとjobのtokenのfile付き（`DAGQ_QUEUE`は渡さない。[Queue service](../queue-service.md#クライアントモード)）で起動し、stdout / stderrを`recovery-<alert>-<attempt>.out` / `.err`に書く（promptと一緒に、promptの固定バイナリの節の全体（claim以後の入れ替えの全てと、読む上限を超えて読まなかった件数）を`recovery-<alert>-<attempt>.binary.json`に書く）。
   timeoutはreviewと同じ。
   上限の件数に達していれば、行き先が待つあいだも、jobを起動せずに`begin_triage`（`recovery_requested`なし）の後すぐ6のescalateにする（`triage_started`の`launch`は行き先のもの、待つあいだは役割のもの）。
   例外は、最後のラウンドの後の人の回答が`edit the task's --verify, then retry_inherit`で、runの最初のeventより後にtaskのverifyを変えた`task_edited`があるときで、上限に達していてもjobをもう1回起動する（ADR-t883-1。直したverifyをjobが読んで`retry_inherit`を選べるように）。
   jobを起動するので、行き先が待つあいだは他のラウンドと同じく待つ。
   その`recovery_requested`は上限を超えた通し番号になる。
   editの無い回答は他のoptionと同じで、上限に達していればすぐescalateになり、同じ選択肢のaskがまた開く（人が直して答えるまで進まない）。
4. **prompt**（`prompt::recovery_prompt`。決定39の入力）: 載せる資料はこの関数のdoc commentが持つ。
   alertの事実の直後に、supervisorの固定バイナリの節を置く（中身は[復旧job](background-recovery-job.md)のprompt）。
   その節に続けて、固定バイナリが依存先の着地を含まないことで落ちたrunの`retry` / `retry_inherit`の選び方を書く（`RECOVERY_BINARY_RULE`）。
   promptは全体（言語の指示を含む）と節ごとの上限の中に収め、終わったrunの資料は先頭を残して切る。
   値・理由・切り方と、切ったものに付ける読む方法は[Prompt](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)、生きているsessionの復旧jobと共通の節は[復旧job](background-recovery-job.md)のpromptが持つ。
   許された操作（`ENDED_ACTIONS`）は5の操作で、baseがすでに受け入れ条件を満たすことの確かめへの引き渡し（`hand_off`）を含む。
   verdictのschemaの後に、jobの足すoptionの種類の宣言と、6で寄せる・外す規則を書く（`RECOVERY_ENDED_OPTIONS_RULE`）。
   引き渡しは、job自身が確かなら次のラウンドに頼むoptionにせず`hand_off`の操作を選ぶように書く。
   verifyを直すoptionはruntimeが足すので、jobには足さずquestionで勧めさせる。
5. **verdictの適用**（`act_on_recovery`）: stdoutからverdictを読む（`RecoveryVerdict::parse`）。
   `confidence: high`の`repair`で、`actions`がちょうど1つ、かつその前提が今成り立つときだけ適用する。
   前提:
   - `retry`: taskの`failed` / `interrupted`のrunがretryの上限未満で、runのbranchに自分のcommitが無い（`result_commit`、無ければworktreeのHEAD、worktreeを作る前に止まったrunなら無し、がmainの祖先。worktreeが消えていて`result_commit`も無ければ判断できないので崩れとする）。
     commitのあるrunを最初からやり直すのは成果の破棄なので適用しない。
     適用するとtaskを`in_progress`から`ready`に戻す（`task_status_changed`）。
   - `retry_inherit`: runのbranchに自分のcommitがあり、そのtaskで引き継ぐretry（`triage_finished`の`action: retry_inherit`）がまだ無い（taskごとに1回。人の手での引き継ぎは数えない。適用の同じトランザクションで再検査する）。
     `refs/dagq/runs/<run-id>`にcommit（`result_commit`、無ければworktreeのHEAD）を残してtaskを`ready`に戻す。
     次のrunは引き継ぎ（`run_inherited`とpromptの「Carried over from run」の節）で始まる。
   - `resume`: resumeを使い切っていない（通算で数える）、worktreeとreceiptのpathがある。
     runを`needs_session`にし、`last_error`にjobの`instruction`（空なら`diagnosis`）を書く。
     [`needs_session`](needs-session.md#needs_session)の自動resumeが拾い、resolution requestは「triageが差し戻した」文面で`instruction`を載せる。
     直前のturnの`peak_context`が閾値を超えていれば、そのresumeは依頼を引き継ぎに入れて新しいsessionで始める（[`needs_session`](needs-session.md#needs_session)の3）。
   - `hand_off`（[ADR-t1818-2](../../adr/2026-10-05-t1818-2-recovery-job-options-map-to-runtime-options-or-pass-back.md)決定3）: workerがcommitせず、`already_satisfied`を書かずにbaseを名指したreceiptで落ちたrunを、[already_satisfiedの検証](validation.md#already_satisfiedの検証)の経路に渡す。
     runが`failed`でtaskの最新のrun（taskは`in_progress`）、最後のresumeより後の最後のvalidationがreceiptの`commit`と確かめたcommitをbaseとする`commit_mismatch`で、runのbranchのHEADがbaseでworktreeにcommitしていない変更が無く（捨てる成果が無い）、receiptがtaskの要るevidenceを裏付ける（`already_satisfied`のreceiptのvalidationと同じ）ときだけ（`domain::recovery::hand_off_refusal`）。
     適用するとrunを`validating`にして`result_commit`と`last_error`を消し（taskの状態と最新のrunであることは同じトランザクションで確かめ直す）、そのrunは検証の始まる前の主張として休む（`satisfied::handed_off`）。
     次のpassの空いたslotで今のmainのsnapshotのverifyが始まり、通ればreview jobが判定し、`high`のpassなら閉じ、それ以外は`approve_close`のaskになる。
     jobは閉じる判断をせず、判定のpromptはworkerが主張を書いていないことと、`triage_finished`の`reason`（jobの見立て）を根拠としてreceiptの確かめと並べて載せる。
     途中でsupervisorが死んだrunのadopterの再validationは、receiptを判定し直さずに主張として受け入れる（`satisfied::handed_off_since_resume`）。
     引き渡しの後にrunがまた`failed`か`interrupted`になれば（人の答えでtaskが動いたときを除く）、そのrunは次のラウンドを持つ（`triage_state`）。
   - `wait`: 何もしない。
     `recheck_at`（上限あり）を過ぎたら、runが同じところに居ればもう一度jobを起動する（alertの回数に数える）。
   - `send_instruction`、`stop_processes`、`answer_known_dialog`、`close_and_proceed` はどの worker にも当てはまらず、ほかの生存 session 専用の操作も終わった run には当てはまらないので、前提の崩れとして扱う。
   適用したら`finish_triage`が遷移と`triage_finished`、`auto_repaired`（`wait`には書かない）、`recovery_finished`を1トランザクションで記録する。
   適用する前にleaseをまだ持っているかを確かめ、失っていれば（止まっていた間に別のsupervisorが引き継いだ）何も書かずにそのrunを手放す。
6. **escalate**: `escalate`、`confidence: low`の`repair`、前提の崩れた`repair`（操作が1つでない、許されない操作を含む）、alertの回数の使い切りは、`kind: decide`のaskをrunに作る（`asked_by: supervisor`）。
   optionsは`retry` / `resume` / `cancel`（resumeを使い切ったrunは`retry` / `cancel`）に、jobの`options`のうち復旧jobに渡し直すものを足し、runの`integrate`の検証が落ちたことがあるrun（`integration_deferred`の`code: verification_failed`）では最後に`edit the task's --verify, then retry_inherit`をruntimeが足したもの（使い切りのaskにも足す）。
   jobのoptionは種類を宣言し、`domain::recovery::sort_job_options`が[ADR-t1818-2](../../adr/2026-10-05-t1818-2-recovery-job-options-map-to-runtime-options-or-pass-back.md)の決定1で分ける（規則の順・種類・語の一覧・印はその関数と定数のdoc comment）。
   runtimeの答えを指すものはそのoptionに寄せてjobの文を推奨としてquestionに載せ、今のaskで適用できないもの・種類の無いもの・runtimeの操作や完了・cancelと紛らわしいものは外す。
   残るjobのoptionは、文の後の印で、optionごとに復旧jobにもう一度渡りruntimeは何も動かさないことを示す。
   baseがすでに受け入れ条件を満たすことの確かめへの引き渡しを次のラウンドに頼むoptionは、文が完了と紛らわしくても外さず、その旨の印で残し、答えは同じく復旧jobに渡る（次のラウンドのjobが5の`hand_off`を選び、前提が崩れていればまたescalateになる）。
   寄せたものと外したものは理由とともにラウンドの記録とquestionに残す。
   questionは各optionの意味を持ち、印の付いたoptionは復旧jobにもう一度渡ること、verifyのoptionを足したaskでは、userかinboxが先にtaskのverifyを直してから答えれば、使い切った後でも復旧jobがもう1回、直したコマンドで走ることを書く。
   askを作った後で`finish_triage`が失敗したら、このpassが開いたaskは取り下げて閉じ、8の失敗にする。
   人にはinboxのwatchが`ask_opened`として知らせる。
   runはそのまま。
   `triage_finished`と`recovery_finished`を記録する。
7. **sessionの停止**: `triage_finished`の後、runが開いたsession（[記録](background-recovery-job.md#runのsessionの記録)）のうち止めた記録の無いもので、backgroundのwrapperがまだ動いているもの（handleのprocessで見る）を止め（どう終わったかは`wrapper_stopped`に記録する。[非対話のworker](headless-worker.md)の「停止の記録」、[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)決定3）、`workspace_closed`を記録する。
   [ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)より前にworkspaceで開いたsessionのIDは開いていないとみなし、閉じない（人が自分のterminalで閉じる）。
   止める失敗は`cleanup_failed`に記録して続ける（`last_error`は変えない）。
   1つでも閉じたら、そのrunの閉じていない`stuck_exit`のaskを閉じる。
   最後にleaseを解放する。
   `triage_finished`の後のこれらの失敗（DBのerrorなど）はsupervisor logに書くだけで、`triage_failed`にはしない（verdictは適用済み）。
   ラウンドとresumeの使い切りと9の適用の後に、runが`failed`かtaskが終わっていれば閉じていない`approve_landing`を閉じる（[Review](review.md#review-supervisor)の6）。
8. **jobの失敗**: 起動できない、非0終了、timeout、stdoutにverdictが無い、適用中のerrorは`triage_failed`（使えなかったproviderなら`provider_unusable`）と`recovery_finished`を記録してleaseを解放し、runはそのままにする。
   `triage_failed`はattentionで、人の指示でinboxのsessionか人の`DAGQ_ROLE`の無いterminalから`ready` / cancelなどを打つ（plannerは`ready`を拒まれる）。
   supervisorは同じrunをもう一度ラウンドにかけない（resumeされれば別。providerが使えずに失敗したjobの`provider_unusable`の付いた`triage_failed`は例外で、attentionにせず次のラウンドを行き先のproviderで始める。[Codexで動かす](#codexで動かす)）。
9. **askの回答**: fill passごとに、`asked_by: supervisor`の`decide`のaskで回答済み・未closeのもののうち、回答がaskのoptionsにある`retry` / `resume` / `cancel`のどれかで、runが`failed` / `interrupted`でleaseが無いものを`decide_triage`で1トランザクションで適用する（runがtaskの最新runで、askが回答済みかつ未closeであることを中で再検査するので、2つのsupervisorが同じ回答を二重に適用しない。staleなleaseの行は同じトランザクションで消すので、止まっていたラウンドのsupervisorが起きてもleaseを更新して書けない。[ADR-0039](../../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)の決定7）。
   `retry`はtaskを`ready`（人の判断なので、commitのあるrunでもよい）、`resume`はrunを`needs_session`（`last_error`は直近の`triage_finished`の`reason`に人がresumeを選んだことを添えたもの）、`cancel`はtaskを`canceled`にし、askを閉じ、`triage_decided`を記録する。
   taskが既に`in_progress`でないか、より新しいrunがあれば（人が`ready`を打ったなど）適用するものが無いのでaskを閉じるだけにする。
   jobの足したoptionは、6の印を外した文だけの回答でもそのoptionとして扱う（`domain::recovery::chosen_option`）。
   回答がjobの足したoptionかruntimeの足した`edit the task's --verify, then retry_inherit`（askのoptionsにあり`retry` / `resume` / `cancel`でないもの）なら、何も動かさずにaskを閉じ、`triage_decided`を記録する。
   `triage_state`はこれを`Pending`と読むので、次のfill passで同じalertのラウンドがもう一度始まり（alertの回数に数える）、promptのalertの事実でjobが人の選んだもの（6の印は外す）を読む。
   verifyのoptionの回答は、verifyを直す`task_edited`が先にあれば3の例外で使い切りの後でもラウンドを持ち、jobはpromptでtaskの今のverifyとその`task_edited`を読んで`retry_inherit`を選べる。
   次のrunの`integrate`はtaskの今のverifyを流す。
   optionsに無い自由な回答は人が読む。
   ただしoptionsのどれとも一致せず、runtimeの操作の名前だけ（復旧jobの操作と`retry` / `resume` / `cancel`。前後の空白・大小・引用符は無視する）の回答は、`answer`が何も書かずに断り、askは開いたまま残る。
   書けば誰も適用しない自由な回答として閉じ、答え直せないため。
   断りのmessageはその名前を含むoptionの原文（そのまま答えればruntimeか復旧jobに渡る）と、`retry_inherit`なら下の人の手での引き継ぎを示す。
   `answer`はこの条件（optionsのどれか）のaskの回答をruntimeが運ぶものとして記録し、attentionにしない。

## 人の手での引き継ぎ

userとinbox（人の言葉で）は、`in_progress`のtaskの最新runが`failed`か`interrupted`のとき、`dagq ready ID --inherit --reason ...`でそのrunのbranchを次のrunに引き継いでtaskを`ready`に戻せる（[ADR-t1962-1](../../adr/2026-10-07-t1962-1-retry-carrying-the-branch-over-by-hand.md)）。
decideのaskの答えに無い引き継ぎを、人の判断として直接打つための操作で、decideの答えの集合（ADR-0047決定3）は変えない。
認可は計画管理の`ready`が行い、引き継ぎの確定は実行と着地が公開するportが行う（境界をまたぐtransactionのT10、`src/context_boundaries/ownership.rs`）。

- 引き継ぐcommitは自動の`retry_inherit`と同じ選び方で決める。
  CLIは確定の前にこれを読み、`refs/dagq/runs/<run-id>`は確定が通った後にだけ書くので、断ったときはtask・ask・refのどれも変えない。
- 前提（taskの状態、runがまだ最新で`failed`か`interrupted`であること、生きたprocessとstaleでないleaseが無いこと、独自のcommitがあること）は確定のトランザクションの中で確かめ、崩れていれば理由つきで断る（独自のcommitが無ければ素の`ready`を案内する）。
- 復旧jobのラウンドとの競合は、`begin_triage`と同じ`BEGIN IMMEDIATE`の確定の中でrunのleaseを見て解く。
  staleでないlease（ラウンドが生きている）があれば断る。
  staleなleaseは同じトランザクションで消すので、止まっていたラウンドはleaseを更新できず、古い候補の`retry_inherit`も含めて何も適用しない。
  確定の後はtaskが`ready`なので、新しいラウンドはそのrunを取らない。
  どちらの順でも、runの引き継ぎは高々1回になる。
- 確定はtaskを`ready`にし（plan reviewを通らない）、runの復旧jobの閉じていない`decide`のaskをruntimeとして閉じ（その答えは誰も適用しない）、`triage_finished`（`action: retry_inherit`）を記録する。
  その`by`（`user`か`inbox`）が手での引き継ぎを自動のものと分け、eventのactorが誰が打ったかを持つ。
- 次のrunは自動の引き継ぎと同じく`run_inherited`とpromptの「Carried over from run」で始まり、promptは人が手で引き継いだことと理由を書く。
- 自動の`retry_inherit`のtaskごとに1回の制限は掛けず、手での引き継ぎはその回数を使わない。

## Codexで動かす

`[roles.recovery] provider = "codex"`の復旧job（[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)の決定1・2・4・5・6、[ADR-t1204-1](../../adr/2026-09-30-t1204-1-explicit-no-claude-operation.md)。providerの項は[Actor model](actor-model.md#provider)）。
jobは状態を変えず読むだけ（`TRIAGE_ACCESS`）で、verdictはClaudeのjobと同じ5・6の検査と適用を通る（許す操作、`confidence: high`の`repair`だけ、前提の再確認、alertごとの回数）。

設定はjobを始めるたびに読み直すので、設定の変更が着地してmain checkoutに反映されればsupervisorの再起動は要らない。

- **行き先**（`Supervisor::recovery_route`）: ラウンドを始める前に決め、`triage_started`の`launch`に書く。
  待つのはjobを起動するラウンドだけで、上限に達したalertのescalateは待たない（2・3）。
  providerを書かない役割はClaudeで、queueのholdのあいだは始めず、[Claudeを使わない運転](../provider-lifecycle.md#claude-を使わない運転)ではjobを起動せずに理由付きの8の失敗にする。
  providerを書いた役割（`claude`を書いたものも）は、そのproviderが使えれば（supervisorにagentがあり控えられていない）そこで、使えなければもう一方のproviderで、どちらも使えなければ待つ。
  `[provider_fallback] jobs = false`なら、使えない理由（実行ファイルが無い・起動できない・認証・利用上限）ではもう一方で始めず、控えが解けるまで待って同じproviderで始める（Claudeを使わない運転による行き先は変わらない。[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）。
  Claudeのholdは`codex`のjobを止めない。
  Claudeを使わない運転でCodexも使えなければ、jobを起動せずに理由付きの8の失敗にし、Claudeに回さない。
- **起動**（`recovery::start_job`）: `triage_started`の`launch`のproviderのagentでjobを起動する。
  Codexでは`codex exec`の読み取りだけのsandboxでrun directoryを作業場所にし、jobの`dagq`がqueue serviceに届くときはjobのpermission profile（`:read-only`を継ぎ、queue serviceのsocketだけに届く）にする（[Provider lifecycle](../provider-lifecycle.md)）。
  書けるroot・sandboxを外すflag・session idは渡さない。
  jobはworkerのworktree・run directoryに書けず、`stop_processes`などの操作はjobではなくruntimeがpidで行う。
- **読み取り**: verdictはCodexの最終の`agent_message`のtext（`AgentProvider::job_reply`）から読む。
  threadはjobの出力から、modelはそのthreadのrolloutから読み（`AgentProvider::job_session`）、`triage_finished` / `triage_failed`と`recovery_finished`に書き、ラウンドの区間の`session_closed`もそれを取る。
- **失敗**: 非0終了・timeout・読めないverdictは、Claudeのjobと同じ8の失敗（`triage_failed`のattention）にし、Claudeに回さない。
  providerが使えずに失敗したjob（ログイン・使用量の上限・agentが起動しない。`JobFailure::switch_reason`）は、役割がproviderを書いていれば（ADR-t1063-1の決定4）そのproviderを控え（`provider_held`。Claudeの壁はqueueのhold askにも加わる）、`triage_failed`に`provider_unusable`を書く。
  この`triage_failed`は人のものではない: 復旧の判定はこれを`Pending`と読み（attentionにもしない）、次のpassで同じrunの次のラウンドが行き先のproviderで始まる（控えの間はもう一方。`jobs = false`なら控えの間は始めず、解けた後に同じproviderで）。
  そのラウンドは新しい`recovery_requested`を持つのでalertの回数に数える。
  Claudeを使わない運転で残るproviderが無ければ、次のラウンドは上の行き先のとおりjobを起動せずに理由付きの8の失敗（attention）になる。
  queueのhold askに加わるのはClaudeのjobの壁だけ（[Queue hold](queue-hold.md)）。
  時間の上限ではjobとCodexが走らせた子孫（別のprocess groupのものも）をpidで止める。

## taskのhold

holdの開いたtaskの止まったrunには復旧jobを起動せず、`begin_triage`の確定の中でもholdを確かめ直す。
holdの前に起動したjobのverdict（確定の中か適用の直前で読む）と`decide`の回答は、taskを進めず`hold_result_kept`に残し、ラウンドは`action: held`で終わる。
解除の後の最初のpassが、版が変わっていなければ適用し、変わっていれば捨てて次のラウンドを待たせる（生きているsessionへのverdictは捨てる）。
詳細は[taskのhold](../../plans/task-hold.md)が持つ。
