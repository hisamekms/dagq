---
id: design-supervisor-lifecycle-background-recovery-job
type: design
title: "生きているsessionの復旧job"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle-task-hold
  - adr-t1566-1
  - design-supervisor-lifecycle-prompt
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-triage
  - adr-0047
  - adr-t609-1
---

# 生きているsessionの復旧job

[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定39・40（task 360、task 441、task 469、task 442、task 562、`application::supervise::recovery`と`stall_recovery`）。復旧jobを、生きているsessionのalertにも広げたもの。runtimeの自動修正（決定25・29など）が直さなかったものを、jobが状況を読んで許された操作で直し、直せないか自信が無いときだけ、今までそのalertがなっていたaskでinboxに上げる。終わったrun（`failed` / `interrupted` / `resume_exhausted`）は[Triage](triage.md#triage-supervisor)が同じverdictで扱う。

## alert

| alert | 条件 | 操作 | escalation |
| --- | --- | --- | --- |
| `stalled` | receipt も開いた質問もなく turn が終わる（`turn_without_receipt`）、または権限拒否が上限に達する（`permission_denied`） | `send_instruction`・`stop_processes`・`resume`・`wait` | `stalled` の ask（`wait` / `stop` と job の選択肢） |
| `idle_process` | run の所有するプロセスが、子孫と合わせて CPU 時間をほとんど使わないまま `idle_process_secs` を超えて生きる | `stop_processes`・`send_instruction`・`wait` | receipt 前は `stalled` の ask、後は段に任せる |
| `failed` | turn が失敗・停止して wrapper が終わった run | `retry`・`retry_inherit`・`resume`・`wait` | triage の ask |

worker の画面由来の `prompt_waiting`・`stuck_exit`、idle marker の対話の `background_tasks` による `long_background`、`stalled` の `send_unconfirmed` は task 1437 で撤去した（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。これらの過去の event・ask・復旧 job は引き続き履歴として読める。画面を読む `ExitWatch` の監視も撤去した。非対話の `idle_process` は残す（ask 394 の回答）。

### CPU時間が伸びないプロセス<a id="cpu時間が伸びないプロセスidle_process"></a>

`ProcessControl::list` が読む一覧を `Supervisor::process_sample` で全 run が共有する。間隔は閾値の10分の1、1〜60秒（test の秒未満の値も使う）。各 watch は1間隔待ってから標本を取り始め、新しい標本の run のプロセスを `domain::idle_process::CpuWatch` へ渡す。CPU 時間の伸びが経過時間の1%を超えれば進んだとし、子が進んでいる親は idle にしない。新しい子は最も近い祖先の進んだ時刻を引き継ぎ、pid の再使用や CPU 時間の減少は新しいプロセスとする。CPU 時間が読めないものは進んでいると扱う。session の起動から60秒以内に始まった agent の子の補助プロセスは除く。

job に渡した idle の部分木は、進むか `wait` の再確認の時刻になるまで再び渡さない。facts は `idle_processes`（pid・ppid・command・elapsed_secs・idle_secs・cpu_ms・cpu_growth_ms・descendants・active_ms）、`threshold: idle_process_secs`・threshold_secs・progress_cpu_per_mille・phase。非対話の session と resume に残る監視を使い、対話の終了待ちの `exit_wait` は使わない。

### 起動とprompt

`recovery_requested` に alert・attempt・evidence・reason と事実と、job の provider・model・effort の `launch`（job には見せない）を記録し、run dir に prompt と stdout / stderr を書く。job は読み取りだけの `DAGQ_ROLE=recovery-job` で、`launch` の provider で起動する（`[roles.recovery] provider = "codex"` なら Codex。下の「Codexで動かす」）。prompt は task・現在の検証・編集の履歴・最後の turn の要約・run のプロセス一覧・worktree の HEAD と receipt の commit・git status と差分を含む。alert の事実の直後に supervisor の固定バイナリの節を持つ（task 1633、`prompt::binary_sections`。資料は `recovery::binary_facts_of` が読み、`prompt::binary_facts` が `BinaryFacts` に組み立てる）: (a) 今の build 識別子とその commit（`Binary of the supervisor (the fixed binary the runtime runs) now: <version> (commit <sha>)`。commit を名乗らないリリースか `unknown` の build はそう書く。節 `binary`、`RECOVERY_BINARY_BYTES` 500 byte）、(c) task の依存先ごとに、最新の `run_integrated` の着地 commit と今の build がそれを含むか（`Repository::is_ancestor`）の JSON の 1 行 `{"held": true | false | null, "landed": <commit> | null, "task": <id>, "why": <言えない理由>}`（`why` は `held` が null のときだけ: 着地していない、build が commit を名乗らない、git の失敗。節 `dependencies`、`RECOVERY_DEPENDENCIES_BYTES` 3,000・1 件 `RECOVERY_DEPENDENCY_BYTES` 400 byte。今の build が含まないか言えないものを ID の昇順で先に、続けて含むものを選ぶ）、(b) run の claim（run の最初の event）より後の固定バイナリの入れ替え: queue の `update_installed`（plugin だけのものは除く）と run の `supervisor_handed_off` を、`event_id`・`kind`・`at`・`previous_version`・`version`（`update_installed` は `commit` も）の JSON の 1 行ずつ古い順に（節 `replacements`。新しい順に `RECOVERY_REPLACEMENTS` 10 件・`RECOVERY_REPLACEMENTS_BYTES` 3,000・1 件 `RECOVERY_REPLACEMENT_BYTES` 400 byte まで選ぶ）。省いたものは件数と ID と、run directory の `recovery-<alert>-<attempt>.binary.json`（`start_job` が prompt と一緒に書く `BinaryFacts` の全体）を読む方法として書く。`retry` / `retry_inherit` を許す job（終わった run の job）には、固定バイナリが依存先の着地を含まないことで落ちた run は、今の build がそれを含めば人に聞かずに `retry`（自分の commit のある run は `retry_inherit`）を `confidence: high` で選んでよく（`TRIAGE_RETRY_FAILURES` の規則で runtime が retry を適用しないときは escalate で retry を推奨する）、含まなければ `wait` か escalate にすることを書く（`RECOVERY_BINARY_RULE`）。prompt は全体の `RECOVERY_PROMPT_LIMIT`（96,000 byte、言語の指示を含む）と節ごとの上限を持ち（task 1571。値と理由は[Prompt](prompt.md#goal-reviewrunのreview復旧jobruntimeのplannerの上限)）、切ったものは run directory と worktree のファイル（claim 時の task は `prompt.txt`、turn の全文は `turns/turn-NNNNNN.jsonl`、終わった run の receipt・検証の log・`terminal-final.txt`、`git status` は worktree）を名指し、ファイルに無いもの（alert の事実・プロセスの一覧・過去の verdict）は読めないと書く。prompt を書いたら `recovery_prompt_written`（`alert`・`attempt`・`prompt_bytes`。plan review と同じ形）を run に記録する（生きている session の job も終わった run の job も `start_job` で）。capture した画面は使わない。`failed` の prompt は triage が組み立てる。


## 適用

high confidence の repair の全操作について前提を確認してから適用し、操作を `auto_repaired`、結果を `recovery_finished`（Codex の job なら thread と model も）に残す。1つでも崩れれば操作せず escalation する。

- `send_instruction`: turn が終わり、次の依頼が無いことを確認して、指示を次の turn の依頼にする。画面・入力欄・Esc で中断された turn・画面から推定した idle には頼らない。
- `stop_processes`: 適用の直前に一覧を読み直し、run の worktree か session の子孫に属する指定 pid だけを止める。wrapper と agent 自身は対象外。SIGTERM の後3秒待ち、生きていればその pid に SIGKILL を送る。終了待ちの時間切れを数え直す処理は撤去した。
- `resume`: 最初の session の run を instruction とともに `needs_session` に park して終了を依頼する。resume の上限を確認する。
- `wait`: alert と reason ごとに再確認を保留する（上限 `MAX_RECHECK_SECS`）。別の alert の job はこの保留を消さない。

`answer_known_dialog`・`close_and_proceed` は worker に適用しない。過去の verdict に書かれていても拒む。


## escalate

`integrate`の検証が落ちたことのある終了run（`integration_deferred`の`code: verification_failed`）の`decide`のaskには、runtimeが`edit the task's --verify, then retry_inherit`を足す（[Triage](triage.md)の6）。verifyそのものが壊れていたら、userまたはinboxが`edit --verify` / `--no-verify`で先に直してから、このoptionで答える。jobに戻った次のroundのpromptにはtaskの現在のverifyと`task_edited`（旧値・新値・actor）が載り、修正済みなら`retry_inherit`を選べる。verifyを直した後のこの回答は、alertの3回を使い切った後でももう1回のroundを持つ（Triageの3）。job自身はeditしない（ADR-t883-1）。

`escalate`、`confidence: low`の`repair`、前提の崩れた`repair`、試行の使い切りは、inbox宛てのそのalertのaskにする（決定40のkind）。optionsはそのkindのものにjobの`options`を足したもの（`recovery::ask_options`。kindのものは`stalled` が `wait` / `stop`（queue が `propose` を足す）。終了runの`decide`では、上のverifyのoptionも）、`reason_category`はjobの`discard` / `scope`、それ以外は`recovery_failed`。questionは今までのaskの文面に、escalateの理由、`Why a person: <reason_category>`、jobの`diagnosis`、推奨の操作（jobの`actions`）、jobの`question`、`recovery-<alert>-<attempt>.prompt.txt`のpathを足したもの。`recovery_finished`（`escalated: true`、`why`、`reason_category`、`ask_id`。Codex の job なら thread と model も）を記録する。

- 対話の run の旧 alert と ask の経路は廃止した。現在の escalation は上の `stalled` と `idle_process`、終わった run の `failed` に当たる。過去の記録はそのまま読む。
- `idle_process`: receipt 前の SessionWatch は stalled の ask に上げる。question は idle なプロセスの pid・command・経過・CPU 時間を示し、StallWatch が答えを扱う。receipt 後と resume は段に任せ、recovery_finished の left_to_phase と diagnosis を記録する。対話の終了待ちの監視は無い。

## jobの失敗

[ADR-t609-1](../../adr/2026-09-27-t609-1-failed-live-recovery-job-opens-the-alert-ask.md)（ADR-0047の決定40をamends、task 562）。生きているrunのどのalertでも、jobの失敗（起動できない、非0終了、timeout、verdictが無いか形に合わない）は、escalateと同じそのalertのask（上の「escalate」のkindとoptions）にする。例外は、`[roles.recovery]`がproviderを書いていて、jobがそのproviderを使えずに失敗した・起動しなかったとき（ログイン・使用量の上限・agentが起動しない。ADR-t1063-1の決定4、task 1225）: そのproviderを控え、askを開かずに`recovery_finished`（`outcome: job_failed`、`escalated: false`、`provider_unusable`）だけを記録し、alertの次のjobを行き先のproviderで始める（`RecoveryWatch::follow_for`と`start`が`restarted`を呼ぶ。`--no-claude`で残るproviderが無ければ、次のjobは起動せずに理由付きのこのaskになる。下の[Codexで動かす](#codexで動かす)）。`recover by hand`のattentionは作らない。`RecoveryWatch::follow_for`はjobの失敗を`Escalation::JobFailed`として返し、各watchがescalateと同じ経路でaskを開く。askの文面の材料（`Escalation::note`）と`recovery_finished`（`Escalation::finished`。`outcome: job_failed`、`error`、`applied: []`）は副作用のない関数で、壊れた出力の形ごとのerrorは`RecoveryVerdict::parse`が決める（unit testで確かめ、`tests/it/runtime_job_verdicts.rs`はalertごとに代表の1つの壊れた出力で配線を確かめる。task 1415）。

- askの`reason_category`は`recovery_failed`で、questionのescalateの理由は`the recovery job failed (<error>)`、`Why a person: recovery_failed`と`recovery-<alert>-<attempt>.prompt.txt`のpathが続く（jobのverdictが無いので`Diagnosis`と`Recommended`は無い）。
- `recovery_finished`は`outcome: job_failed`、`error`、`escalated: true`、`ask_id`、`reason_category: recovery_failed`。`recovery_failed`のeventは記録しない。
- 終わったrunの[Triage](triage.md)の失敗（`triage_failed`、`triage by hand`）は変えない。

ADR-t609-1より前のruntimeは、`stalled`以外のalertのjobの失敗を`recover by hand`のattentionにしていた（`recovery_finished`の`outcome: job_failed`と、`recovery_failed`のevent（`code: job_failed`、`alert`、`attempt`、`error`、`workspace_id`、`reason_category: recovery_failed`））。そのruntimeが記録してまだ解消していない`recovery_failed`（`domain::recovery::failed_live`）は、adoptしたruntimeも読む: `status`はleaseのある生きているrunでもそのrunのattention（`recover by hand`、`kind: recovery_failed`、`last_error`はjobのerror、`last_error_code: job_failed`）を出し、そのalertのjobは立てない。解消するのは、その後に`session_exited`・`workspace_closed`・`supervision_finished`・`run_recovered`・`runtime_error`か、`prompt_waiting`なら`prompt_cleared`、`long_background`なら`receipt_observed`、`stalled`なら`receipt_observed`か`detection: recovery`の`stall_resolved`が記録されたとき。

## Codexで動かす

`[roles.recovery] provider = "codex"`の生きているrunのjob（task 1225。終わったrunのjobと同じ役割・同じ行き先で、仕組みとこの repositoryの設定は[Triage](triage.md#codexで動かす)）。区間は持たない（`recovery_requested`の`launch`と`recovery_finished`が記録）。

- **行き先**（`Supervisor::recovery_route`）: alertのjobを始めるとき決め、`recovery_requested`の`launch`に書く。providerを書かない役割はClaudeで、queueのholdのあいだは始めない（alertは解けてから追い直す）。providerを書いた役割（`claude`を書いたものも）は使えるproviderで始め、Claudeのholdは`codex`のjobを止めない。`[provider_fallback] jobs = false`なら、使えない理由ではもう一方で始めず、控えが解けるまで待って同じproviderで始める（`--no-claude`による行き先は変わらない。[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）。`--no-claude`で動かせるproviderが無ければjobを起動せず、jobの失敗と同じそのalertのask（`recovery_failed`、`the recovery job could not start: provider_disabled: …; handle this role manually`）にする。
- **起動と検査**: Codexでは`codex exec --json`の読み取りだけのsandbox（queue serviceに届くjobのprofile）で、verdictは最終の`agent_message`から読み、上の「適用」と同じ検査（alertごとに許す操作、`confidence: high`の`repair`だけ、適用の直前の前提の再確認、alertごとの3回）を通る。jobはworktree・run directory・cmuxのworkspaceに書けず、`stop_processes`はruntimeがpidで行う。
- **失敗**: 非0終了・timeout・読めないverdictは上の「jobの失敗」と同じそのalertのask（`recovery_failed`）にし、Claudeに回さない。providerが使えずに失敗した・起動しなかったjob（ログイン・使用量の上限・agentが起動しない）は、役割がproviderを書いていれば（ADR-t1063-1の決定4）そのproviderを控え（`provider_held`）、askを開かずに`recovery_finished`（`outcome: job_failed`、`escalated: false`、`error`、`provider_unusable`）だけを記録する。alertの次のjobは次の見張りで行き先のprovider（控えの間はもう一方、`jobs = false`なら控えが解けた後の同じprovider）で始まり（`idle_process`は渡したプロセスをもう一度alertにできるよう戻す）、alertの3回に数える。`--no-claude`で残るproviderが無ければ、次のjobは起動せずに理由付きのalertのaskになる。queueのhold askに加わるのはClaudeのjobの壁だけ（壁で止まったClaudeのjobは今までどおりaskにもしない）。
- **記録**: `recovery_finished`（適用・escalate・失敗のどれでも）にCodexのthreadの`session_id`と`model`（読めなければ`model_unknown`）を書く。escalateの`recovery_finished`は`Escalation::record`が書くので、終わったjobのthreadとmodelは`Supervisor::live_job_ends`にrun・alert・attemptごとに置いてそこで載せる。`headless_jobs`の`provider`は`launch`のprovider。
- **test**: `tests/it/recovery_codex.rs`（stalledのrunのjobの`resume`が適用されて着地すること、`confidence: low`の`repair`と失敗したjobが`recovery_failed`のaskになること、ログインで失敗したCodexのjobがaskを開かず次のjobがClaudeで動いて着地すること）と、`domain::actor_model`のunit test（`jobs`のon / offとCodex・Claudeを書いた・書かない役割の行き先）。

## 引き継ぎ

adopt は stalled の reason ごとの wait の recheck_at_ms と、turn の依頼の履歴を読み、同じ依頼を二重に届けない。過去の long_background / send_unconfirmed / stuck_exit の監視は復元しない。idle_process の CPU 観測はメモリだけなので最初の標本から数え直す。前の supervisor が走らせていた job は引き継がず、記録済みの pid と開始時刻でその子を片付けてから新しい job を立てる。handoff も job を止めてから引き継ぐ。

receiptの形式は`src/domain/views.rs`の`Receipt`で、promptとREADMEに同じ契約を書いている。

```json
{"run_id": "...", "result": "succeeded | failed", "commit": "full SHA",
 "tests": {"status": "passed | failed | not_applicable", "evidence_or_reason": "..."},
 "e2e": {...}, "subagent_review": {...}, "summary": "..."}
```

## 非対話のrun

workerのrunはすべて非対話で（[ADR-t813-1](../../adr/2026-09-28-t813-1-headless-worker-path.md)、task 815。task 1437から対話のrunは無い）、画面・ダイアログ・`/exit`に由来するalert（`prompt_waiting`・`stuck_exit`）と`answer_known_dialog`・`close_and_proceed`はどのrunにも出ない。その代わりの`stalled`（理由`turn_without_receipt` / `permission_denied`、操作は`send_instruction`・`stop_processes`・`resume`・`wait`）と、turnの失敗・停止で終わったrunの`failed`（`retry`・`retry_inherit`・`resume`・`wait`）の扱いは[非対話のworker](headless-worker.md#復旧jobのalertと操作決定9)にある。復旧jobは画面の代わりに最後のturnの要約を読む。

## taskのhold（予定・未実装）

holdの開いたtaskの生きているrunには、holdの待ちに入った後は新しいalertのjobを起動しない（holdの待ちはstall・idleの検知の対象にしない）。
holdの前に起動したjobのverdictは、hold中は適用せず記録として残し、解除の後に版が変わっていなければ適用し、変わっていれば捨てる。
決定は[ADR-t1879-1](../../adr/2026-10-07-t1879-1-hold-a-task-at-the-turn-boundary-and-release-it-explicitly.md)、詳細は[taskのhold](task-hold.md)の「列の外の結果と回答の適用」が持つ。
現行の挙動は上の各節のとおり。
