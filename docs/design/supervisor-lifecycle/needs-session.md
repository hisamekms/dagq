---
id: design-supervisor-lifecycle-needs-session
type: design
title: "`needs_session`"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t639-1
  - adr-t946-1
  - adr-t3255-1
  - design-supervisor-lifecycle
  - adr-0027
  - adr-0026
  - adr-0028
  - adr-0047
  - adr-0068
  - adr-t803-1
  - adr-0079
  - design-supervisor-lifecycle-worker-model
---

# `needs_session`

rebaseの衝突と再検証の失敗（宣言外のパスを含む、[`integrate`](integrate.md#receiptの検査とrebase)）は`defer_integration`でrunを`needs_session`にし、理由を`last_error`、詳細を`integration_deferred`イベントに書いて、lease行を消しスロットを空ける。
worktreeは衝突なら検証済みhead、再検証の失敗ならrebase済みのheadに置いたまま残す。
検証コマンドの失敗のうちhostの分類は`needs_session`にせず、integrateが同じ試行で1回やり直し、なお落ちればrunを`awaiting_integration`に戻して（`integration_held`）inboxに知らせる（[`integrate`](integrate.md#hostの失敗のやり直し)）。
resumeの回数は使わない。
runはTaskを占有し続け、`ready`/`cancel`はできない。

今のsupervisorは`session_gone_parked`を書かず、`/exit`が届かなかったrunを判定し直して`needs_session`にする経路も無い。
過去に記録された`session_gone_parked`は履歴として読むだけで、parkのイベントとして扱われる。
resumeの依頼文は`ResumeKind::SessionGone`（理由の示すものを片付けてcommitし、receiptを書き直す）になる。
resumeの継ぎの判定とstatsの再試行の理由はparkのイベントとして数え、`last_error`を説明するイベントにも含まれる。

reviewがpassしたrunのうちe2eが要るものは、着地の前にruntimeがhostで流したe2eが変更のせいで落ちると`needs_session`になる（[ADR-t1233-2](../../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)、[着地の前のe2e](integrate.md#着地の前のe2e)）。
parkのイベントは`run_e2e_failed`で、resumeの依頼文は`ResumeKind::E2e`（reasonのlogを読んで落ちたtestを直してcommitし、再現は名前で絞った1本だけ）、試行は上限に数え、modelは上げない。
resumeが解消すれば、`integrate`が呼ばれていないrunはvalidatingとreviewをやり直し、着地の前に新しいcommitのe2eをもう一度流す。

着地待ちのrun（`awaiting_integration`）は、着地の後のlanding recheck（[landing recheck](integrate.md#landing-recheck)、[ADR-0068](../../adr/0068-recheck-waiting-runs-after-each-landing.md)）が衝突かコードの分類の失敗を見つけたときも`needs_session`になる（commandがhostの分類で落ちたときと、containerのrunでcommandを流すcontainerが無かったときはならない、[ADR-t2424-1](../../adr/2026-10-17-t2424-1-a-container-runs-landing-recheck-command-runs-in-the-verification-container.md)）。
parkのイベントは`landing_recheck_failed`で、resumeの依頼文は`ResumeKind::Recheck`、試行の数え方ではcode `rebase_conflict`ならreviewのverdictを問わず衝突だけのresumeになる（下の「試行の数え方」。使い切ったときの引き継ぐretryはreviewがpass済みかapproveされたrunだけ）。
resumeが解決したrunに閉じていない`approve_landing`のaskがあれば、`validating`の後にreviewをやり直さず、sessionを閉じて答えを待つ。

resumeで開くworkerのsessionは、直前のsessionのmodel / effortを引き継ぐ。
runを`needs_session`にした最新のeventのcodeが`verification_failed`（integrateかlanding recheckの検証の失敗）か`sent_back`（`approve_landing`の`send_back`）なら1段上げ、衝突（`rebase_conflict`）・kill・`evidence_missing`・`scope_violation`などでは上げない。
`begin_resume`が`resume_started`に選んだmodelと、上げたなら前の値と理由を書く（[Worker model](worker-model.md#段上げ決定5)、[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)の決定5）。

`integrate`は呼ばれた時点で、runに`integration_approved`イベントを1回だけ記録する。
supervisorが承認済みのrunを着地させるときは、この`push`に従ってmainを`origin`へpushする（`integrate`と同じ`push_main`）。
`integrate`を呼ぶことが着地の承認で（ADR-0016の決定5）、runtimeが自分で着地させるのは承認済みのrun、reviewがpassしたrun（上限の後の差し戻しが収束した`revise_limit_judged`も含む。[Review](review.md#reviewの流れ)の4a）、`approve_landing`に`land`と答えたrunだけ（[ADR-0027](../../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）。

**試行の数え方**（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定24）: 判定はdomainの`domain::resume`がrunのイベントだけから行う。
`resume_started`ごとに、それより前のイベントで (a) reviewがpass済み（着地の承認か、passか収束したreview）で、(b) runを`needs_session`にした最新のイベントが`code: rebase_conflict`の`integration_deferred`なら、その試行は**衝突だけの試行**で、上限に数えない。
それ以外（検証の失敗、`evidence_missing`、`scope_violation`、人の`send_back`、triageのresume、reviewがpassしていない衝突）は数える。
着地前の衝突の確認（[Review](review.md#review-supervisor)の5のprecheck、ADR-0027の決定4）が生きているsessionに送った解消依頼（後で取り消したものを除く）も、reviewがpassした後の衝突だけなので衝突だけの試行で、上限に数えない。
衝突だけの試行（衝突だけのresumeとprecheckの依頼の和）は別に`dagq.toml`の`[resume] conflict_only_limit`（書式は[Run environment](run-environment.md)）で止める（同じ衝突が解けないまま回り続けないための柵）。
使い切りの判定と、askやlast_error・復旧jobのpromptの文面は、supervisorが起動時に読んだ設定の値を使う。
数える試行が上限に達したか、衝突だけのresumeが衝突だけの上限に達したrun、または衝突だけで`needs_session`になっている（次のresumeが衝突だけの試行になる）runで衝突だけの試行（precheckの依頼を含む）が衝突だけの上限に達したものは、試行を使い切っている。
precheckの依頼で衝突だけの上限に達したrunでも、検証の失敗・`evidence_missing`・`scope_violation`など別の理由でparkしたなら、数える試行が残っていればresumeする。
人の`send_back`（`approve_landing`の`send_back`）で`needs_session`になったrunは、衝突だけのresumeが衝突だけの上限に達していても、数える試行が残っていればresumeする（[ADR-t2946-1](../../adr/2026-10-22-t2946-1-a-persons-send-back-resumes-past-the-conflict-only-limit.md)）。
この例外はsend_backの直後の1回のresumeだけで、そのresumeは数える試行に数え、その後に衝突や別の理由でparkすれば衝突だけの上限で今どおり使い切りになる。
triageのresumeの上限と`resumes_left`は数える試行だけを見る。
precheckは、数える試行が上限に達していればaskにし、衝突だけの試行が上限に達していれば依頼を送らずに着地へ進め、着地のrebaseの衝突で`needs_session`になったrunは試行を使い切っているので下の「引き継ぐretry」に進む（引き継げなければprecheckがaskにする）。

**killだけの試行**（[ADR-t946-1](../../adr/2026-09-29-t946-1-kill-only-resumes-have-their-own-limit.md)）: 外からのsignalでsessionが終わって（`session_killed`）runが失敗し、復旧jobの`resume`か人の`decide`の`resume`で`needs_session`に戻ったrunの次の`resume_started`は、それより前のrunの`last_error`が`session_killed`なら、上限に数えない。
`begin_resume`は同じトランザクションで`auto_repaired`を記録し、試行の通し番号・数えるかとそのpayloadはイベントから`domain::resume::resume_start_plan`が組む。
killだけの試行は別の上限（設定にしない）で止め、達したrunは試行を使い切っている（復旧jobの`resume`も拒まれ、`resume_exhausted`として下の「復旧jobに渡す」に進む）。
killだけの試行はkillの後の`resume`の直後の1回だけで、そのresumeのsessionが解消せずに終わって次のresumeに回るなら（間に新しいkillが無ければ）数える試行になる。
また数えるかどうかは衝突だけの試行と同じくresumeの直前の`needs_session`の理由で決まるので、killの後のresumeが検証の失敗などでparkすれば次のresumeは数える試行、reviewがpass済みの衝突なら衝突だけの試行になる。
使い切ったrunの`recovery_requested`・`last_error`・復旧jobのpromptの文面はkillだけの試行とproviderだけの試行の数も書く。

**holdの後の試行**（[ADR-t2664-1](../../adr/2026-10-22-t2664-1-resumes-after-a-hold-release-are-not-counted.md)）: runを`needs_session`にした最新のparkがholdの待ち（`hold_parked`）なら、解除の後の次の`resume_started`は上限に数えず、killだけの試行と同じく`auto_repaired`で記録する。
holdの待ちにあるrunは、数える試行が上限に達していても試行を使い切ったものとして扱わず、解除の後にresumeする（holdの試行には上限を設けない。解除は人の操作でしか起きない）。
holdの試行は`hold_parked`の後の最初のresumeだけで、その後か、resumeの前に別の理由でparkし直したrunは、その理由で数える。

**providerだけの試行**（[ADR-t3255-1](../../adr/2026-10-24-t3255-1-provider-only-resumes-wait-for-the-hold-and-are-not-counted.md)）: turnがproviderの壁だけで終わり、commitもreceiptの書き直しも無く終わったresumeは、sessionの終わり方を問わず数える試行にも衝突だけの試行にも数えず、別の上限（設定にしない）で止める（`domain::resume::finish_provider_only`）。
衝突だけの試行とproviderだけの試行の後のresumeは、providerの控えが解けるか移れるまで始めず（precheckの依頼も）、待ちは数えない（`domain::resume::waits_for_provider`）。

supervisorは`needs_session`のrunを自分でresumeする（ADR-0019の決定1。`resume_candidates`）。
ループの各回で、adoptの後、`needs_session`のrunを古い順に見て再開の候補を集める。
候補は復旧job・claimと1つの列で、taskの効く優先度の降順（同じ優先度では再開が先、再開の中は古い順）に空きslotを受ける（[supervise](supervise.md#slotを渡す順とclaim)、[ADR-t1850-1](../../adr/2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)）。
人とinboxは`set-priority`で再開待ちのtaskの優先度を変えて、次の再開の順を変えられる（走っている再開は止めない）。
試行を使い切ったrun（上の「試行の数え方」）、staleでないleaseを持つrun、前のsessionのwrapperのプロセスがまだ生きている（未終了でPIDが生きている。heartbeatが止まっていても）runは飛ばす。
このsupervisorが見ていないsessionと同じworktreeで2つ目のsessionを始めないため。
このsupervisorがまだslotで進めているrun（leaseが他のtokenに移ってstaleになり、sessionが終わっていても）も飛ばす: そのslotは次のtickでleaseを失ったとして外れ（[supervise](supervise.md#provisionと監視)）、その後のpassでだけ他のrunと同じく次の試行を始める（同じrunを2つのslotで進めない）。
resumeの前に、このrunに記録されたresumeのsession（backgroundのwrapperのhandle）のうち止めた記録の無いもの（手放したsession、errorで残したもの）がまだ動いていれば止め、`workspace_closed`を記録する。
動いているかはhandleのprocessで見る（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。
ADR-t1433-3より前にworkspaceで開いたresumeのsessionのIDは動いていないとみなし、閉じずに人に任せる（人が自分のterminalで閉じる）。
resume中に死んだsupervisorが起動したwrapper（handleの記録が無い）は見つけないが、wrapperが生きている間は上の条件で飛ばす。
`dagq.toml`がworkerをcontainerで動かすときは、containerが使えないか、containerの届かないproviderで動くrunを候補から飛ばし、hostで再開しない（[Container runtime](../container-runtime.md#fail-closedの境界)）。
試行を使い切ったrunを復旧jobに渡すことは止めない。

0. **解消済みの確認**（`resolved_head`）: resumeを始める前に、前の試行で解消済みかを確かめる。
   前の試行のsessionがreceiptを書き直した後に判定されたrunは、`rewritten_receipt`がresume開始より新しいreceiptを要求するので`unresolved`になり試行を空費する。
   次をすべて満たせばsessionを開かない（`skips_resume`）: (a) 人の`send_back`（指摘への対応はsessionが要る）でないparkの後にsessionが`unresolved`で終わった、(b) receiptがこのrunの`succeeded`で要求を満たし、(c) cleanなworktreeのHEADを名指し、(d) HEADが`main`を真の祖先に持つ。
   どれかが読めない・満たさないときは1に進む。
   宣言外のパスはここでは見ず、skipの後の[Validation](validation.md#validation)か着地が検査する。
   満たせば1トランザクションで`needs_session`でleaseが無いかstale、heartbeatする未終了のprocessが無ければleaseし、`resume_skipped`を記録する。
   `resume_started`は記録しないので試行回数は増えず、試行が上限に達したrunには行わない。
   `integration_approved`があればrunは`needs_session`のまま着地スロットを待ち、下の5と同じく着地させる。
   無ければrunを`validating`にし、sessionの無いまま[Validation](validation.md#validation)→[Review](review.md#review-supervisor)に進める。
   承認済みの`resume_skipped`の後に着地・review・resumeの開始、着地の決定、`approve_landing`のaskが無ければ、`landing_queued`が無くても（答えが届かず）着地スロットを待ち、組み立て直してもresumeし直さない（[supervise](supervise.md#queue-serviceの経路)）。
   着地の`integration_deferred`、着地スロット待ちのerror、supervisorの死で着地待ちから外れると、最後が`resume_skipped`かparkのイベントなので(a)が成り立たず、次はsessionでresumeする。
   未承認でskipしたrunを持つsupervisorが`validating`か`awaiting_integration`のうちに死んだときは、wrapperが無いか生きていなくても、最後のresumeのイベントが`resume_skipped`ならadoptする。
   そのrunのsessionは無いものとして扱う。
   `resume_skipped`はattentionにしない。
1. **開始**: `begin_resume`が1トランザクションで、runがまだ`needs_session`で試行を使い切っておらず、leaseが無いかstale（staleなら置き換える）で、heartbeatしている未終了のprocessが無いことを確かめ、runをこのsupervisorのtokenでleaseし、`supervisor_token`を移し、前のsessionの`run_processes`の行を消し（履歴はイベントに残る）、`resume_started`を記録する。
   runを`needs_session`にした最後のイベントが`scope_violation`か、`scope_violation`を持つ`integration_deferred`なら、下の3の依頼を宣言パス用の文面にする。
   その最後のイベントが`evidence_missing`か、`checks`を持つ`integration_deferred`（上の要求evidenceで着地しなかった）なら、下の3の依頼をevidence用の文面にする。
   runの状態は`needs_session`のままで、新しい状態は足さない。
   進行は`resume_started` / `resume_finished`で読む。
2. **起動**: worker と同じ非対話の wrapper を起動する。
   runtime の snapshot を置き換え、resume の wrapper を background で起動する（`SessionWrappers::launch_background`。[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)。run は workspace を開かない）。
   handle を `workspace_created` で記録し、`wrapper_launched` を記録してから、wrapper の登録を待つ。
   記録できなければ wrapper を止めてエラーにし、起動できなければ `backend_call_failed` を run に記録して下の6の error になる。
3. **解消依頼**: wrapper が登録されたら、定型の解消依頼を次の turn の request file として書き、resume_request_sent を記録する。
   起動中の入力欄や固定の paste 待ちは使わない。
   依頼は現在の task と検証、前の失敗と修正すべき理由を含む。
   理由が上限を超えるときは先頭だけを載せ、全文を run dir の file に書いてその path を示す（[Prompt](prompt.md#次のturnの文の上限)）。
   直前のturnの`peak_context`が`[fresh_session]`の閾値を超えれば、2の起動の前に`session_renewed`を記録し、wrapperは前のsessionを開き直さず新しいsessionを依頼を持つ引き継ぎで始める（`resume-<attempt>.txt`はその引き継ぎ。[Agent provider lifecycle](../provider-lifecycle.md#大きなcontextからの新しいsession)）。
4. **監視と終了**: 依頼より新しい receipt と turn の終了、wrapper の生存を確認する。
   worker_question の回答は次の turn として届け、stalled の待ちも slot の外で保持する。
   非対話の idle_process の復旧は残す。
   解消・失敗・未解消で終了を依頼し、wrapper の終了を確認する。
   時間の判断（wrapperの登録の待ち、依頼か最後の入力からの`resume_timeout`、終了の依頼からのexit timeout）は注入した`Clock`で測る。

5. **その後**: `resolved`で`integration_approved`があれば（0の解消済みの確認で`resume_skipped`になった承認済みのrunも）、`landing_queued`を記録し、leaseを持ったまま着地スロットが空くのを待ち（他のrunが`integrating`なら次の回に回す）、`begin_integration`（同じtokenのleaseはそのまま使う）から`integrate`と同じ関数を別threadで走らせる。
   承認済みのrunは、receiptのheadがbranchのheadと一致すれば同じトランザクションでそれを着地するcommit（`result_commit`）にし、一致しなければ理由を残して`unresolved`として扱う。
   着地がまた`needs_session`になればleaseは外れ、次の回に1に戻る。
   `integration_approved`が無ければ、上の4のとおりsessionを開いたままrunを`validating`にしてreviewに進む（resumeしたsessionが自分で終わっていた場合も同じ経路で、wrapperはreviewの後に止める）。
   `failed`ならrunを`failed`にする。
   `unresolved`は`needs_session`のままleaseを外し、試行が残っていれば次の回に再びresumeする。
6. **打ち切りとerror**: 試行を使い切った後の`resume_finished`が未解消で、次の`resume_candidates`でそのrunにstaleでないleaseも生きているwrapperも無ければ（slotの空きに関わらず）、supervisorは次の**引き継ぐretry**か、復旧jobに渡すかを決める。

   **引き継ぐretry**（ADR-0047の決定24）: runが今も衝突だけで`needs_session`（上の(a)(b)がすべてのイベントについて成り立つ）で、同じtaskのどのrunもこのretryをしていなければ（taskごとに1回まで。人の手での引き継ぎ（[Triage](triage.md#人の手での引き継ぎ)）は数えない。`inherits_on_exhaustion`）、人に聞かずにretryする。
   runの`result_commit`（着地するcommit（5）。無ければ、rebaseの途中でないworktreeのHEAD。登録の壊れたworktreeのrunは[triageの`retry`の前提](triage.md#triage-supervisor)と同じく読む）がbaseと違えば、それを`refs/dagq/runs/<run-id>`に向け（branchを消されても残るように）、1トランザクションで下と同じ確認と、衝突だけで止まっていることとtaskごとに1回までであることの再確認をしてrunを`failed`にし、taskを`in_progress`から`ready`に戻し（中身は変えないのでplan reviewにはかけない）、`auto_repaired`と`triage_finished`を記録する。
   askは開かない。
   残したresumeのwrapperはtriageと同じく止め、worktreeとbranchはtaskが`ready`なので残る。
   次のrunのclaimでsupervisorは、前のrunのうち最も新しい引き継ぐretryで終わったrunがあれば（直前のrunが早くに失敗してtriageでretryされても引き継ぐ）、worker promptに引き継ぎの節（前のrunのreviewはpassしたが衝突で試行を使い切ったこと、commit（`refs/dagq/runs/<id>`とbranch）と、そのrun自身のcommitの範囲（headと新しいrunのbaseのmerge-baseから。途中までrebaseしたresumeがmainのcommitを下に積んでいても、それを含めない）、今のmain（新しいrunのbase）に載せ直して衝突を解き、検証を再実行して自分のheadでreceiptを書くこと、前のreceiptのpathとsummary）を足し、新しいrunに`run_inherited`を記録する。
   commitがbaseと同じ（載せ直すものが無い）か、refを作れなければ、次の「復旧jobに渡す」に進む。
   2回目の使い切りも復旧jobに渡す。

   **復旧jobに渡す**（`exhaust_resumes`。ADR-0047の決定39）: 1トランザクションで、runがまだ`needs_session`で試行を使い切り、taskが`in_progress`でrunがその最新で、staleでないleaseが無いことを確かめ、runを`failed`にし（`last_error`は元の`last_error`に試行の回数を添え、衝突だけの試行やprecheckの依頼があればその内訳と上限も書く）、`recovery_requested`（`alert: resume_exhausted`）を記録する。
   `triage_finished`は書かないので、[Triage](triage.md#triage-supervisor)の復旧jobがこのalertでrunを取る（resumeはもう選べない）。
   続けて残したresumeのwrapperを止める。
   wrapperの起動・依頼の書き込み・heartbeat切れなどresume自体のerrorは`resume_finished`（`outcome: error`）とsupervisor logに書き、leaseを外す。
   runの`last_error`（衝突の理由）は変えず、試行は1回と数える。
   resumeのwrapperは、leaseを外した時点で未登録（leaseが無いのでもう登録できない）か終了済みなら止め（起動の途中で失敗してhandleが返らなければ何もしない）、生きていれば調査のため残す（`resume_finished`にhandleを記録する）。
   残したwrapperは上の条件で次の試行を止めるが、このrunの`resume_finished`に記録されたhandleは、runのwrapperがもう生きていなければ次の回に止めて次の試行を始める。
   着地スロットを待つ間のerrorでは`resume_finished`は記録済みなので、leaseだけを外す。
   resume中にsupervisorが死んだ場合は下の「supervisorの入れ替わり」。

resumeのwrapperを起動するのはsupervisorだけ。
`status`のattentionは、[放棄](background-recovery-job.md#1-runの放棄abandon)の例外を除き、`needs_session`のrunなら`next: resuming (runtime)`（resume中、試行が残っている、生きているsessionが次の試行を止めている、resume中のsupervisorが死んだ、試行を使い切って復旧jobへ移る直前のいずれもsupervisorのもの）。
`kind`はrunを`needs_session`にした最新のイベント。

セッションが変更不要と判断した場合はreceiptを`result: failed`と理由（`summary`）で書き直す。
`integrate ID`は`fail_integration`でrunを`failed`にし（`integration_failed`）、mainには触れない。
worktreeは残る。
`failed`になったrunはsupervisorがtriageする（[Triage](triage.md#triage-supervisor)。triageのresumeは`needs_session`に戻し、ここの試行の上限を通算で共有する）。

## 再計画で保留したrun

保留したrunは`needs_session`（`replan_parked`）になってresume/reviseの回数を引き継ぎ、保護中は自動resume・`resolved_head`・使い切りの後始末の対象にならない。
snapshotを保存した保留は、継続か案の撤回で操作が閉じると照合してから続ける（[順と失敗](../../plans/task-replanning.md#実装の状態)）。
hostのrunは制御側がresumeか後始末の前に照合し（無ければ作る）、containerのrunはresumeのagentの前にexecutorが照合し制御側がslotで記録し、使い切れば失敗とし、失敗は`replan_restore_failed`で操作を`blocked`に戻す。
保留を終えた柵（`blocked`などの保留の後の操作、落ち着いたhold）がslotに見つけたrunは、書き手の終わりを待ってから、snapshotを作らずworktreeを残して休む（`ParkFor::Rest`）。
この休止は照合も作り直しもせず、操作の結末か解除の後のresumeが新しいvalidationとreviewから続ける。
保留の時にturnが始まっていなかった回答は継続のresumeの依頼に載せ、柵に拒まれて配れなかった回答は継続のsessionが届ける。

## supervisorの入れ替わり

- 解消依頼と終了依頼は file に書く。
  過去の記録と handoff の欄は読める。

引き継がないもの: wrapperが終了を記録したか死んでいるrunは、`resume_run`がstaleなleaseを置き換えて次の試行を始める。
試行の`workspace_created`が無い（wrapperを起動してhandleを記録する前にsupervisorが死んだ）runは監視する手段が無いので引き継がず、sessionが終わるまで次の試行を止める（人が wrapper の生存を確認する）。
`resume_request_sent`を記録しないバイナリが始めたresumeを引き継ぐと、依頼を送ったかが分からないので、次の turn の依頼としてもう一度書く。
依頼を書いてから`resume_request_sent`を記録するまでの間にsupervisorが死んだときも同じく2回目が送られる。

## taskのhold

holdの開いたtaskの`needs_session`のrunはresumeせず、使い切ったresumeの後始末（`exhaust_resumes`）もしない。
再開の候補の列から外して`hold_deferred`に残し、resumeの開始・skip・使い切りの確定の中でもholdを確かめ直す。
slotで工程が走っていたrunは、その工程の終わりで再計画と同じ保留を通って`needs_session`（`hold_parked`）になり、sessionとworktreeを残す（[非対話のworker](headless-worker.md#再計画の保留)）。
保留の時にturnが始まっていなかった回答は、同じrunをresumeする解除（`task_hold::release_decision`が`Resume`を返すもの）に限り解除の後のresumeの依頼に載せ、保留より前の承認とreviewのpassは、そのresumeの後の着地に使わない。
runを終えて新しいrunを始める解除（`release_decision`が`RetryInherit`・`RetryFresh`を返すもの。分け方は下の`task_hold::ends_run`の段落）では、届いていない答えは終えたrunに残り、新しいrunの依頼には載らない。
同じproviderで続く解除は印を外すだけで、次のpassが今のresumeの経路（`begin_resume`。大きなcontextからの新しいsessionの判定もそこで効く）で続ける。
runを終える解除は2つに分かれる（`task_hold::ends_run`）。
止まっていない待ちのrun（`needs_session`など）は、別のproviderを指定した解除か、sessionの無いrun（providerが会話を見つけないと言った後に、答えたturnも名指したsessionも途中で切れたturnも無い。wrapperの起動だけでは無いまま）の解除で終わり、sessionの無いことでrunを終えるのはこの場合に限る。
止まった（`failed`・`interrupted`）runは、providerかmodelを指定した解除（modelだけでも）か、残した結果の無いrunの解除で終わり、残した結果がありproviderもmodelも指定しない解除はsessionの有無にかかわらずrunを終えずに残した結果を適用して進む。
runを終える解除は、runを終えてtaskをreadyに戻し、次のrunが、runのbranchにあるworktree（containerのrunは最後のturnの終わりの記録）の今のheadを検証済みのcommitに依らずに（worktreeが無ければ検証済みのcommitを）、commitに保存した未commitの差分とともに引き継ぐ（`task_hold::release_decision`、記録は`task_released`の`decision`と手での`retry_inherit`）。
引き継ぐものが無ければ次のrunは今のmainから始め、それより前のrunの引き継ぎも辿らない（`resume::inherited_from`）。
未commitの差分を保存できないとき（containerのrunはcloneでgitを走らせないので、記録が事実だけのturnの後も最後のturnの終わりのものでないか、未commitの差分・途中のrebase・bundleに無いheadを示すとき）と、worktreeのHEADがrunのbranchに無いときと、引き継ぐheadを`refs/dagq/runs/<run-id>`に残せないときは、解除を拒んでhold・run・taskを変えない（refは確定の直前に書く）。
成果は確定の外で読むので、確定の中で読んだ時の開いたholdとrunの進行の版（`task_hold::ProgressVersion`）を照らし、食い違えば（間に別の解除・resume・holdが入った）同じく拒む（`release_read_moved`）。
再計画の保留とholdが重なれば、両方が終わるまでresumeしない。
resumeかtriageの途中でsupervisorが死に、staleなleaseのまま休んだrunは、holdがadoptもresumeも止めるので、別のsupervisorがholdのparkのためにそのleaseを引き継ぎ（`lease_acquired`の`hold_park`）、slotのrunと同じ保留で書き手の終わりを待ってから待ちに入れる。
