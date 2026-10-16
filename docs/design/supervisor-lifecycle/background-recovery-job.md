---
id: design-supervisor-lifecycle-background-recovery-job
type: design
title: "生きているsessionの復旧job"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1566-1
  - design-supervisor-lifecycle-prompt
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-triage
  - adr-0047
  - adr-t609-1
---

# 生きているsessionの復旧job

[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定39・40（`application::supervise::recovery`）。
復旧jobを、生きているsessionのalertにも広げたもの。
runtimeの自動修正（決定25・29など）が直さなかったものを、jobが状況を読んで許された操作で直し、直せないか自信が無いときだけ、今までそのalertがなっていたaskでinboxに上げる。
終わったrun（`failed` / `interrupted` / `resume_exhausted`）は[Triage](triage.md#triage-supervisor)が同じverdictで扱う。

## alert

| alert | 条件 | 操作 | escalation |
| --- | --- | --- | --- |
| `stalled` | receipt も開いた質問もなく turn が終わる（`turn_without_receipt`）、または権限拒否が上限に達する（`permission_denied`） | `send_instruction`・`stop_processes`・`resume`・`wait` | `stalled` の ask（`wait` / `stop` と job の選択肢） |
| `idle_process` | run の所有するプロセスが、子孫と合わせて CPU 時間をほとんど使わないまま `idle_process_secs` を超えて生きる | `stop_processes`・`send_instruction`・`wait` | receipt 前は `stalled` の ask、後は段に任せる |
| `failed` | turn が失敗・停止して wrapper が終わった run | `retry`・`retry_inherit`・`resume`・`wait` | triage の ask |

対話の経路の alert（worker の画面に由来するものと、idle marker の対話の `background_tasks` に由来するもの）は無い（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。
過去のそれらの event・ask・復旧 job は履歴として読める。

### CPU時間が伸びないプロセス<a id="cpu時間が伸びないプロセスidle_process"></a>

プロセスの一覧は全 run が共有する。
各 watch は1間隔待ってから標本を取り始め、新しい標本の run のプロセスを `domain::idle_process::CpuWatch` へ渡す。
CPU 時間の伸びが経過時間の一定の割合を超えれば進んだとし、子が進んでいる親は idle にしない。
session の起動の直後に始まった agent の子の補助プロセスは除く。

job に渡した idle の部分木は、進むか `wait` の再確認の時刻になるまで再び渡さない。
非対話の session と resume に残る監視を使い、対話の終了待ちの `exit_wait` は使わない。

### 起動とprompt

`recovery_requested` に alert と事実と、job の provider・model・effort（job には見せない）を記録し、run dir に prompt と stdout / stderr を書く。
job は読み取りだけの役割で、記録した provider で起動する（下の「Codexで動かす」）。
prompt は task・現在の検証・編集の履歴・最後の turn の要約・run のプロセス一覧・worktree の HEAD と receipt の commit・git status と差分を含む。
alert の事実の直後に、supervisor の固定バイナリの節（今の build とその commit、task の依存先ごとの着地を今の build が含むか、run の claim より後の固定バイナリの入れ替え）を持つ。
`retry` / `retry_inherit` を許す job（終わった run の job）には、固定バイナリが依存先の着地を含まないことで落ちた run は、今の build がそれを含めば人に聞かずに retry を選んでよく、含まなければ `wait` か escalate にすることを書く（`RECOVERY_BINARY_RULE`）。
prompt は全体と節ごとの上限を持ち（[Prompt](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)）、切ったものは run directory と worktree のファイルを名指し、ファイルに無いもの（alert の事実・プロセスの一覧・過去の verdict）は読めないと書く。
prompt を書いたら、生きている session の job も終わった run の job も `recovery_prompt_written` を run に記録する。
capture した画面は使わない。
`failed` の prompt は triage が組み立てる。

## 適用

high confidence の repair の全操作について前提を確認してから適用し、操作を `auto_repaired`、結果を `recovery_finished`（Codex の job なら thread と model も）に残す。
1つでも崩れれば操作せず escalation する。

- `send_instruction`: turn が終わり、次の依頼が無いことを確認して、指示を次の turn の依頼にする。
  画面・入力欄・Esc で中断された turn・画面から推定した idle には頼らない。
- `stop_processes`: 適用の直前に一覧を読み直し、run の worktree か session の子孫に属する指定 pid だけを止める。
  wrapper と agent 自身は対象外。
  SIGTERM の後に猶予を待ち、生きていればその pid に SIGKILL を送る。
- `resume`: 最初の session の run を instruction とともに `needs_session` に park して終了を依頼する。
  resume の上限を確認する。
- `wait`: alert と reason ごとに再確認を保留する（上限 `MAX_RECHECK_SECS`）。
  別の alert の job はこの保留を消さない。

`answer_known_dialog`・`close_and_proceed` は worker に適用しない。
過去の verdict に書かれていても拒む。

## escalate

`integrate`の検証が落ちたことのある終了runの`decide`のaskには、runtimeがverifyを直してから`retry_inherit`するoption（`VERIFY_FIX_OPTION`）を足す（[Triage](triage.md)の6）。
verifyそのものが壊れていたら、userまたはinboxがtaskのverifyを直してから、このoptionで答える。
jobに戻った次のroundのpromptにはtaskの現在のverifyと`task_edited`が載り、修正済みなら`retry_inherit`を選べる。
verifyを直した後のこの回答は、alertごとの上限を使い切った後でも次のroundを持つ（Triageの3）。
job自身はeditしない（ADR-t883-1）。

`escalate`、`confidence: low`の`repair`、前提の崩れた`repair`、試行の使い切りは、inbox宛てのそのalertのaskにする（決定40のkind）。
optionsはそのkindのものにjobの`options`を足したもの（`recovery::ask_options`。終了runの`decide`では、上のverifyのoptionも）。
questionは今までのaskの文面に、escalateの理由とjobのverdictと復旧の材料のpathを足したもの。
escalateしたことを`recovery_finished`に記録する（Codex の job なら thread と model も）。

- `idle_process`: receipt 前は stalled の ask に上げ、StallWatch が答えを扱う。
  receipt 後と resume は段に任せ、そのことを recovery_finished に記録する。

## jobの失敗

[ADR-t609-1](../../adr/2026-09-27-t609-1-failed-live-recovery-job-opens-the-alert-ask.md)（ADR-0047の決定40をamends）。
生きているrunのどのalertでも、jobの失敗（起動できない、非0終了、timeout、verdictが無いか形に合わない）は、escalateと同じそのalertのask（上の「escalate」のkindとoptions）にする。
例外は、`[roles.recovery]`がproviderを書いていて、jobがそのproviderを使えずに失敗した・起動しなかったとき（ログイン・使用量の上限・agentが起動しない。ADR-t1063-1の決定4）である。
そのときはそのproviderを控え、askを開かずに`recovery_finished`だけを記録し、alertの次のjobを行き先のproviderで始める（[Claudeを使わない運転](../provider-lifecycle.md#claude-を使わない運転)で残るproviderが無ければ、次のjobは起動せずに理由付きのこのaskになる。下の[Codexで動かす](#codexで動かす)）。
`recover by hand`のattentionは作らない。
`RecoveryWatch::follow_for`はjobの失敗をescalationとして返し、各watchがescalateと同じ経路でaskを開く。
askの文面の材料と`recovery_finished`は副作用のない関数が組み立てる。

- askの理由は`recovery_failed`で、jobのverdictが無いのでverdictの部分は無い。
- `recovery_finished`はjobの失敗として記録し、`recovery_failed`のeventは記録しない。
- 終わったrunの[Triage](triage.md)の失敗（`triage_failed`、`triage by hand`）は変えない。

ADR-t609-1より前のruntimeが記録してまだ解消していない`recovery_failed`（`domain::recovery::failed_live`）は、adoptしたruntimeも読む。
`status`はleaseのある生きているrunでもそのrunのattention（`recover by hand`）を出し、そのalertのjobは立てない。

## Codexで動かす

`[roles.recovery] provider = "codex"`の生きているrunのjob（終わったrunのjobと同じ役割・同じ行き先で、仕組みは[Triage](triage.md#codexで動かす)）。
区間は持たない（`recovery_requested`と`recovery_finished`が記録）。

- **行き先**（`Supervisor::recovery_route`）: alertのjobを始めるとき決め、`recovery_requested`に書く。
  providerを書かない役割はClaudeで、queueのholdのあいだは始めない（alertは解けてから追い直す）。
  providerを書いた役割（`claude`を書いたものも）は使えるproviderで始め、Claudeのholdは`codex`のjobを止めない。
  `[provider_fallback] jobs = false`なら、使えない理由ではもう一方で始めず、控えが解けるまで待って同じproviderで始める（Claudeを使わない運転による行き先は変わらない。[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）。
  Claudeを使わない運転で動かせるproviderが無ければjobを起動せず、jobの失敗と同じそのalertのask（`recovery_failed`）にする。
- **起動と検査**: Codexでは`codex exec`の読み取りだけのsandbox（queue serviceに届くjobのprofile）で、verdictは最終のmessageから読み、上の「適用」と同じ検査（alertごとに許す操作、`confidence: high`の`repair`だけ、適用の直前の前提の再確認、alertごとの上限）を通る。
  jobはworktree・run directory・cmuxのworkspaceに書けず、`stop_processes`はruntimeがpidで行う。
- **失敗**: 非0終了・timeout・読めないverdictは上の「jobの失敗」と同じそのalertのask（`recovery_failed`）にし、Claudeに回さない。
  providerが使えずに失敗した・起動しなかったjob（ログイン・使用量の上限・agentが起動しない）は、役割がproviderを書いていれば（ADR-t1063-1の決定4）そのproviderを控え（`provider_held`）、askを開かずに`recovery_finished`だけを記録する。
  alertの次のjobは次の見張りで行き先のprovider（控えの間はもう一方、`jobs = false`なら控えが解けた後の同じprovider）で始まり（`idle_process`は渡したプロセスをもう一度alertにできるよう戻す）、alertごとの上限に数える。
  queueのhold askに加わるのはClaudeのjobの壁だけ（壁で止まったClaudeのjobはaskにもしない）。
- **記録**: `recovery_finished`（適用・escalate・失敗のどれでも）にCodexのthreadとmodelを書く。
  escalateの`recovery_finished`は別の経路で書くので、終わったjobのthreadとmodelは`TriageState::live_job_ends`にrun・alert・attemptごとに置いてそこで載せる。
  `headless_jobs`の`provider`は`recovery_requested`のprovider。

## 引き継ぎ

adopt は stalled の reason ごとの wait の再確認の時刻と、turn の依頼の履歴を読み、同じ依頼を二重に届けない。
過去の対話の監視は復元しない。
idle_process の CPU 観測はメモリだけなので最初の標本から数え直す。
前の supervisor が走らせていた job は引き継がず、記録済みの pid と開始時刻でその子を片付けてから新しい job を立てる。
handoff も job を止めてから引き継ぐ。

receiptの形式は`src/domain/receipt.rs`の`Receipt`で、promptとREADMEに同じ契約を書いている。

## 非対話のrun

workerのrunはすべて非対話で（[ADR-t813-1](../../adr/2026-09-28-t813-1-headless-worker-path.md)）、画面・ダイアログ・`/exit`に由来するalertと`answer_known_dialog`・`close_and_proceed`はどのrunにも出ない。
その代わりの`stalled`と、turnの失敗・停止で終わったrunの`failed`の扱いは[非対話のworker](headless-worker.md#復旧jobのalertと操作決定9)にある。
復旧jobは画面の代わりに最後のturnの要約を読む。

## 後片付けの境界と約束

containerで動くrunの後片付けは、worktreeのrunと同じ契機で、clone・制御側のrun branch・着地用のworktree・run dirのrun branchのbundleと状態のファイル・mainのbundleに及ぶ（[ADR-t2179-1](../../adr/2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)決定7）。
約束は2つで、cloneではgitを走らせず、workerが置いたsymlinkの先（hostのファイルとdir）を消さない。
片付けはgitを使わない削除とmainのrepositoryのgitの手順に分かれる。
clone・着地用のworktreeのdir・run dirのbundleと状態のファイルの削除はgitを使わずguardを要らないので、guard（決定8）が成り立たなくても行う。
制御側のrun branchの削除と着地用のworktreeの登録の解除はguardを通したgitでだけ行い、guardが成り立たなければ飛ばして片付けの失敗として残し、guardが成り立った後の片付けのやり直しが消す。
復旧jobとtriageがrunのheadと状態を読むのもcloneではなくbundleと状態のファイルからで、読めなければその旨を事実として渡す。
片付ける物の列挙と手順は`supervise::cleanup`の`clean_clone`と`application::run_git::remove`のdoc commentが持つ。
