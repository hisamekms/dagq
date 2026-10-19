---
id: design-supervisor-lifecycle-background-recovery-job
type: design
title: "runの復旧（復旧job・receiptの無いidle・1 runの放棄・後片付け）"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1566-1
  - design-supervisor-lifecycle-prompt
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-triage
  - adr-0047
  - adr-0054
  - adr-t609-1
  - adr-t1433-3
  - adr-t1818-2
---

# runの復旧（復旧job・receiptの無いidle・1 runの放棄・後片付け）

## 目的

runの異常を、runtimeの自動修正、復旧job、人へのaskの3段で扱う（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)）。
この文書は、生きているrunの復旧job、receiptの無いturnの検知、runtime errorで1 runだけを手放すこと（abandon）、終わったrunのsessionの後片付けの地図である。
終わったrun（`failed` / `interrupted` / `resume_exhausted`）の復旧jobは[Triage](triage.md#triage-supervisor)が同じverdictで扱う。

## 流れ

```text
生きているrun
  ├─ turnがreceiptも開いた質問も無く終わる → 促す → 使い切ればstalledの復旧job
  ├─ 権限拒否が上限に達する → 促さずにstalledの復旧job
  ├─ runのプロセスがCPU時間を伸ばさない → idle_processの復旧job
  │    └─ job: high confidenceのrepairを前提を確かめて適用 / それ以外はそのalertのaskでinboxへ
  └─ 監視中のruntime error → そのrunだけを手放す（abandon）
       └─ sessionが残っていなければ次のpassでrecover → interrupted → triage
終わったrun → wrapperが残っていればtriageの後か次の掃除が止める（worktreeは状態に応じて残す）
```

## 責務と境界

- 生きているrunの復旧jobの起動・verdictの検査と適用・escalateは`application::supervise::recovery`（module docが範囲を持つ）、turnのstallからjobへの受け渡しとresumeのrepairのparkは`stall_recovery`。
- receiptの無いturnの検知（促し・stalledのask・`stall_resolved`）は`application::supervise::stall`の`StallWatch`、判断は同じmoduleの副作用のない関数が持つ。
- 1 runを手放すのは`Supervisor::abandon`と`abandon_with_session`（`mod.rs`）で、記録はqueueの`abandon_run`。
- 終わったrunのsessionの掃除は`sweep`（`SweepWatch`・`sweep_ended_sessions`）、worktreeとビルド成果物の掃除は`cleanup`（module docが範囲を持つ）と[Run worktrees](run-worktrees.md)。
- 終わったrunの復旧jobとtriageの後のsessionの停止は[Triage](triage.md)、非対話のworkerのturnの後の扱いは[非対話のworker](headless-worker.md)が持つ。
- 判断と境界の分け方は[ADR-t1410-1](../../adr/2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)に従う。
  見張りの次の一手と引き継いだ見張りの組み立て直しは副作用のない関数が値（時刻、receipt・質問・holdの有無）を受けて決め、queue・file・sessionの読み書きは結果を実行する薄い処理だけが行う。

## 不変条件

- 1 runの異常は他のrunに波及しない（leaseはrun単位。[ADR-0054](../../adr/0054-run-lease-ownership-parallel-supervisors-and-recover.md)の決定4）。
- 復旧jobは読み取りだけの役割で、操作はruntimeが前提を確かめてから適用する。
  1つでも前提が崩れれば操作せずescalateする。
- 生きているrunの復旧jobの失敗は、escalateと同じそのalertのaskになり、`recover by hand`のattentionを作らない（[ADR-t609-1](../../adr/2026-09-27-t609-1-failed-live-recovery-job-opens-the-alert-ask.md)）。
- 検知ごとの結末（`stall_resolved`）は1回だけ記録され、recover・abandon・triage・sweepが終える検知も同じ共通処理で記録する。
- 画面・打鍵・Enterの確認・capture した画面を、検知にも復旧jobにも使わない（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。

## 生きているrunの復旧job

runtimeの自動修正が直さなかったものをjobが状況を読んで許された操作で直し、直せないか自信が無いときだけ、そのalertのaskでinboxに上げる（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定39・40）。

- alertは`stalled`（receiptの無いturn、権限拒否の上限）、`idle_process`（CPU時間が伸びないプロセス）、終わったrunの`failed`で、alertごとに許す操作は`recovery`とtriageのverdictの検査が持つ。
- 対話の経路のalert（workerの画面に由来するもの、idle markerの対話の`background_tasks`に由来するもの）は無い。
  過去のそれらのevent・ask・復旧jobは履歴として読める。
- `answer_known_dialog`・`close_and_proceed`はworkerに適用せず、過去のverdictに書かれていても拒む。

### CPU時間が伸びないプロセス<a id="cpu時間が伸びないプロセスidle_process"></a>

- プロセスの一覧は全runが共有し、判断は`domain::idle_process::CpuWatch`が持つ。
- 子が進んでいる親はidleにせず、sessionの起動の直後に始まったagentの子の補助プロセスは除く。
- jobに渡したidleの部分木は、進むか`wait`の再確認の時刻になるまで再び渡さない。
- 非対話のsessionとresumeに残る監視を使い、対話の終了待ちは使わない。
- receipt前は`stalled`のaskに上げてStallWatchが答えを扱い、receipt後とresumeは段に任せる。

### 起動とprompt

- 起動の入口は`recovery::start_job`、promptの中身は`prompt::run_review::recovery_prompt`のdoc commentが持つ。
- jobは画面の代わりに最後のturnの要約を読む。
- alertの事実の直後に、supervisorの固定バイナリの節（今のbuildが依存先の着地を含むか、claimより後の入れ替え）を置く（`binary_facts_of`）。
  `retry`を許すjobには、固定バイナリが依存先の着地を含まないことで落ちたrunは今のbuildが含めば人に聞かずにretryしてよく、含まなければ`wait`かescalateにする規則を書く（`RECOVERY_BINARY_RULE`）。
- verdictのschemaの後に、jobの足すoptionの種類の宣言と寄せる・外す規則を書く（生きているsessionは`RECOVERY_LIVE_OPTIONS_RULE`、終わったrunは`RECOVERY_ENDED_OPTIONS_RULE`。下の「escalate」）。
- promptの上限と切り方は[Prompt](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)が持ち、切ったものはファイルを名指し、ファイルに無いもの（alertの事実・プロセスの一覧・過去のverdict）は読めないと書く。
- `failed`のpromptはtriageが組み立てる。

### 適用

- 適用は`recovery::apply_live`で、high confidenceのrepairの全操作の前提を確かめてから適用し、1つでも崩れればverdict全体を拒んでescalateする。
- 操作ごとの前提は`check_live`のdoc commentが持つ。
  落とし穴は、どの操作も画面・入力欄・画面から推定したidleを前提にしないことと、止めるのはrunのプロセスだけでwrapperとagent自身は止めないことである。

### escalate

- `escalate`、`confidence: low`の`repair`、前提の崩れた`repair`、試行の使い切りは、inbox宛てのそのalertのaskにする（決定40のkind）。
- askの選択肢の組み立ては`recovery::ask_options`が持ち、askはそのkindのものに、jobの選択肢のうち残るものを足すので、人はjobの見立てと元のaskの答えの両方から選べる。
- jobの選択肢は終わったrunと同じ規則で寄せるか外す（[ADR-t1818-2](../../adr/2026-10-05-t1818-2-recovery-job-options-map-to-runtime-options-or-pass-back.md)の決定1、[Triage](triage.md)の6、`domain::recovery::sort_job_options`）。
  `stalled`のaskでは`wait` / `stop`がruntimeの答えで、引き渡しを頼むoptionは出さない。
  残るjobの選択肢は、印でoptionごとに復旧jobにもう一度渡りruntimeは何も動かさないことを示し、寄せたものと外したものは理由とともにjobの終わりの記録とquestionに残す。
- 印の付いたoptionの答え（印の無い文だけでもよい）は、runのslotの中で`StallWatch`がaskを閉じて記録し、sessionには何も送らない。
  そのalertの次の復旧jobは、alertがまた起きたとき（turnのstallは次の見張り、`idle_process`は渡したプロセスを戻した次の標本）に始まり、その答えを読む（`domain::recovery::live_pass_back`）。
  jobの回数はalertの上限に数え、使い切ればaskがまた開く。
  それ以外の自由な文は今までどおりsessionの次のturnとして送る。
- `integrate`の検証が落ちたことのある終了runの`decide`のaskには、verifyを直してから`retry_inherit`するoption（`VERIFY_FIX_OPTION`）を足す（[Triage](triage.md)）。
  verifyを直すのはuserかinboxで、job自身はeditしない（ADR-t883-1）。
  直した後のこの回答は、alertごとの上限を使い切った後でも次のroundを持つ。

### jobの失敗

- 生きているrunのどのalertでも、jobの失敗（起動できない、非0終了、timeout、verdictが無いか形に合わない）は、escalateと同じそのalertのaskにする（[ADR-t609-1](../../adr/2026-09-27-t609-1-failed-live-recovery-job-opens-the-alert-ask.md)）。
- 例外は、役割がproviderを書いていて、jobがそのproviderを使えずに失敗した・起動しなかったとき（ADR-t1063-1の決定4）で、そのproviderを控え、askを開かずに`recovery_finished`だけを記録する。
- `RecoveryWatch::follow_for`はjobの失敗をescalationとして返し、各watchがescalateと同じ経路でaskを開く。
- 終わったrunのtriageの失敗（`triage by hand`）はこれと別で、[Triage](triage.md)が持つ。
- 過去のruntimeが記録してまだ解消していない`recovery_failed`（`domain::recovery::failed_live`）は、adoptしたruntimeも読み、そのrunのattention（`recover by hand`）を出してjobを立てない。

## Codexで動かす

`[roles.recovery]`がproviderを書いた生きているrunのjobで、終わったrunのjobと同じ役割・同じ行き先である（仕組みは[Triage](triage.md#codexで動かす)）。

- 行き先は`Supervisor::recovery_route`がalertのjobを始めるときに決め、`recovery_requested`に書き、headless jobの記録のproviderもそれである。
  providerを書かない役割はClaudeで、queueのholdのあいだは始めない。
  providerを書いた役割は使えるproviderで始め、Claudeのholdは`codex`のjobを止めない。
  providerの切り替えを切った設定では、控えが解けるまで待って同じproviderで始める（[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）。
  [Claudeを使わない運転](../provider-lifecycle.md#claude-を使わない運転)で動かせるproviderが無ければjobを起動せず、jobの失敗と同じaskにする。
- Codexのjobは読み取りだけのsandboxで動き、上の「適用」と同じ検査を通り、worktree・run directoryに書けず、`stop_processes`はruntimeがpidで行う。
- 非0終了・timeout・読めないverdictはjobの失敗と同じaskにし、Claudeに回さない。
- providerを控えた後のalertの次のjobは行き先のproviderで始まり、alertごとの上限に数える（`idle_process`は渡したプロセスをもう一度alertにできるよう戻す）。
- queueのhold askに加わるのはClaudeのjobの壁だけである。

## receiptの無いturn

workerはすべて非対話のturnで動き（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）、turnの後の扱いは[ADR-t813-1](../../adr/2026-09-28-t813-1-headless-worker-path.md)の決定9に従う。
turnの後の扱いの詳細は[非対話のworker](headless-worker.md#turnの後の扱い)が持つ。

- 対象は、最初のsessionのturnがreceiptも未回答の`worker_question`も残さずに終わったときである。
  resumeは解消依頼のturnの後にreceiptを、reviseは差し戻しのturnの後に書き直しを判定する。
- turnの終わりが決まるので閾値を待たずに促し、促しの上限を使い切ったら`stalled`の復旧jobにする。
  権限拒否が上限に達したら促さずに復旧jobにする。
- `stalled`のaskの選択肢は`intervene`の代わりに`stop`を持ち、questionは答える前にturnの記録を読むことを書く。
  答えごとの扱いは`stall.rs`の`AskStep`が持つ。
- 答えずに閉じた質問（`closed_undelivered`）は、閉じたことを伝える文を次のturnとして1回だけ届ける。
  引き継いだsupervisorも同じ文を二度届けない。
- 時刻の比較で、同じミリ秒の印を新しいものと取らない理由は`file_time`のmodule docが持つ。
- 画面由来の過去の検知と閾値の記録は読め、statsは過去の検知と結末を引き続き集計する。
  新しい検知はどのsessionの画面からもidleを判定しない。

## 1 runの放棄（abandon）<a id="1-runの放棄abandon"></a>

- 監視中のruntime error（wrapperのプロセスも死んでいるheartbeat切れ、検証処理そのもののGitやDBの失敗、closeの記録の失敗など）は、そのrunだけを手放す。
  wrapperのプロセスが生きていればheartbeat切れでも手放さない（[黙ったwrapper](headless-worker.md#黙ったwrapper)）。
- 手放すと`runtime_error`を書いてleaseを消し、status・runのプロセス・worktreeは変えず、supervisorは他のrunを続ける。
  同じtransactionで、そのrunの開いた`stalled`のaskを閉じ、結末の無い検知に`stall_resolved`を1回ずつ記録する。
- leaseを消すのは、常駐supervisorが生きている間も`recover`がrunのprocessだけで判定できるようにするためである。
  taskは未完了のrunで占有されたままなので二重実行にならず、leaseの無い未登録のwrapperは登録できずagentを起動せず、他のsupervisorも引き継がない。
- `status` / `watch`はこのrunを`recover run`のattentionとして出す（[ADR-0054](../../adr/0054-run-lease-ownership-parallel-supervisors-and-recover.md)の決定8・12）。
  sessionのprocessが残っていなければ、次のfill passでsupervisor自身がrecoverして`interrupted`にしtriageに回す（ADR-0047の決定3・ADR-0054の決定11）。
  processが生きている間は人を待ち、人は`dagq-recover` skillに従う（`recover`を人が打つのはsupervisorが居ないときだけで、居ればprocessが止まった時点でsupervisorが行う）。
- verdictの後の終了の待ちと最初のsessionの終了の依頼には時間切れが無く、abandonせずleaseを持ったままsessionの終了を待つ（resumeのsessionだけはexit timeoutで手放す。[`needs_session`](needs-session.md)）。
- supervisorがworkerのsessionを開いたまま持っている間（validating・review・revise・verdictの後の終了の待ち）に手放すときは、手放す前にそのsessionに終了を依頼する（`exit_abandoned_session`）。
  手放したrunはleaseが無く、supervisorもadoptも見ないので、送らなければsessionは誰も見ないまま開いて残る。
- 依頼を書けなかったなら、そのrunのattentionは`exit the session`（`domain::AttentionNext::ExitSession`）になり、人がそのsessionを終える。
  sessionの終了が記録されると、runは元のattentionに戻る。
  置き換えるのは`review and integrate`と`recover run`だけで、`needs_session`・`failed`のrunは`resuming (runtime)`・`triaging (runtime)`のまま出る。
- validatingとreviewの工程は、errorで手放すときにsessionが分かるよう、次の工程に移るまでsessionを工程に残しておく。

## 後片付け

- sessionの終了はsupervisorが行う（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。
  終わらせるのは終了の依頼と、残っていればwrapperを止めること（[ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)の決定3）だけである。
- `awaiting_integration`のrunは、reviewのverdictの後にsessionの終了を待ってwrapperだけを止め（reviseの間は止めない。[`supervise`](supervise.md#検証とreviewと着地)）、worktreeとbranchは`integrate`が着地するまで残す。
- `failed`、provisioningや検証処理のエラー、wrapper heartbeat切れのrunは、worktreeを調査のため残し、ビルド成果物だけを消す（[Run worktrees](run-worktrees.md#run-worktrees)）。
  調べる材料はrun dirのlogとturnで、`dagq run log`で読む。
- `failed`・`interrupted`のrunにまだ動いているwrapperがあれば、triageの後にsupervisorが止める（[Triage](triage.md#triage-supervisor)）。
- supervisorの後始末はsupervisorが動いている間だけ働き、runtimeはrunのworkspaceを開かないので、過去に作られて残ったworkspaceは人が閉じる。
  workspaceで開いた古いrunのsessionは、supervisorは閉じない。

### runのsessionの記録<a id="runのsessionの記録"></a>

- runが開いたwrapperは、最初・resumeごと・開き直しのどれもrun_eventsに残り、止めたかはwrapperごとに判じる（`domain::run::run_workspaces`）。
  `session_renewed`はwrapperを増やさないので、止めるのは区切りの前と同じwrapperである。
- reviseは生きているwrapperに送るので、wrapper（process）は開き直さない。
- ただし直前のturnの`peak_context`が`[fresh_session]`の閾値を超えれば、同じwrapperが`session_renewed`の後に新しい名前のモデルのsessionを始める（[Agent provider lifecycle](../provider-lifecycle.md#大きなcontextからの新しいsession)）。

### 終わったrunの掃除<a id="終わったrunの掃除"></a>

- 人が`ready` / cancelした後のrunや着地したrunに残ったwrapperは、fill passの掃除（`sweep_ended_sessions`）が止める。
  対象は、生きたsupervisorのleaseが無く、slotに居らず、triageの対象でないrunである。
- staleなleaseは無いものとして扱う。
  runを終わった状態にしてからleaseを放すまでにsupervisorが死ぬと、leaseが残ってwrapperが永久に拾われなくなるためである。
- 正は記録ではなくwrapperのprocessで、動いていないsessionには何も書かないので、同じrunを何度掃除しても記録は増えない。
- 止められなければ失敗を残して次の掃除で止め直し、DBのerrorでもループは止めない。
- worktreeとbranchは同じ回の[Run worktrees](run-worktrees.md)の掃除が扱い、run dirは消さない。

### 後片付けの境界と約束

- containerで動くrunの後片付けは、worktreeのrunと同じ契機で、clone・制御側のrun branch・着地用のworktree・run dirのbundleと状態のファイル・mainのbundleに及ぶ（[ADR-t2179-1](../../adr/2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)決定7）。
- 約束は2つで、cloneではgitを走らせず、workerが置いたsymlinkの先（hostのファイルとdir）を消さない。
- gitを使わない削除はguard（決定8）が成り立たなくても行い、制御側のrun branchの削除と着地用のworktreeの登録の解除はguardを通したgitでだけ行う。
  guardが成り立たなければ飛ばして片付けの失敗として残し、成り立った後の片付けのやり直しが消す。
- 復旧jobとtriageがrunのheadと状態を読むのもcloneではなくbundleと状態のファイルからで、読めなければその旨を事実として渡す。
- 片付ける物と手順は`supervise::cleanup`の`clean_clone`と`application::run_git::remove`のdoc commentが持つ。

## 引き継ぎ

- adoptはstalledのreasonごとのwaitの再確認の時刻とturnの依頼の履歴を読み、同じ依頼を二重に届けない。
  検知の結末の判断もrun_eventsの記録から組み立て直す。
- `idle_process`のCPUの観測はメモリだけなので、最初の標本から数え直す。
- 前のsupervisorが走らせていたjobは引き継がず、記録済みのpidと開始時刻でその子を片付けてから新しいjobを立てる。
  handoffもjobを止めてから引き継ぐ。
- receiptの形式は`src/domain/receipt.rs`の`Receipt`で、promptとREADMEに同じ契約を書いている。
