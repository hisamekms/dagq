---
id: design-supervisor-lifecycle
type: design
title: Supervisor and workspace lifecycle
status: current
created: 2026-09-21
scope: runtime
related:
  - design-supervisor-lifecycle-throughput-review
  - design-supervisor-lifecycle-host-metrics
  - design-supervisor-lifecycle-actor-model
  - design-supervisor-lifecycle-goal-review
  - design-supervisor-lifecycle-commands
  - design-supervisor-lifecycle-claim-hold
  - design-supervisor-lifecycle-claim-defer
  - design-supervisor-lifecycle-disk-space
  - design-supervisor-lifecycle-queue-hold
  - adr-0044
  - adr-0047
  - adr-0038
  - adr-0002
  - adr-0003
  - adr-0006
  - adr-0007
  - adr-0008
  - adr-0009
  - adr-0010
  - adr-0011
  - adr-0012
  - adr-0039
  - adr-0013
  - adr-0014
  - adr-0045
  - adr-0016
  - adr-0018
  - adr-0019
  - adr-0020
  - adr-0021
  - adr-0022
  - adr-0040
  - adr-0024
  - adr-0025
  - adr-0026
  - adr-0027
  - adr-0028
  - adr-0029
  - adr-0031
  - adr-0037
  - adr-0043
  - design-persistence
  - design-provider-lifecycle
  - design-plugin-integration
---

# Supervisor and workspace lifecycle

```text
ready task (dependencies completed)
  → claim TaskRun + run lease      ─┐
  → create Git worktree              │ up to the parallel limit at once,
  → start session wrapper            │ each with this state machine
  → start Claude/Codex               │
  → running                          │
  → completion receipt, session idle │ (the session stays open)
  → validate receipt, commit, clean state (own thread)
      already_satisfied (no commit) → stays validating, no landing: the task's
        verification on a snapshot of main → approve_close ask → close (verified
        again if main moved) → task completed without a commit; retry / cancel
  → awaiting_integration: program reviews, the binary's built-ins then the
    landing branch's [review.programs], one at a time
      first non-zero exit → revise (no agent starts); start failure / timeout → review failed
  → headless review (Claude, or Codex read-only when
    [roles.review] says provider = "codex"; an agent's job on its
    [review.subagents.<agent>] provider when set) → verdict
      pass    → exit request → close workspace → land (single slot, push)
      revise  → fixed request to the live session → rewritten receipt
                → validate → review again (at most MAX_REVISE_ATTEMPTS revises a run)
      concern → the job's sure land / send_back applied, else exit request → close → approve_landing ask
      revise or send_back past them → no code finding: follow_up drafts → land like a pass
                                    → a code finding: exit request → close → approve_landing ask
      no answer from a job   → that job once more (review_job_finished; once per job)
      review failed          → review span closed → exit request → close → approve_landing ask + review_failed
  → release run lease               ─┘
  → integrate (by hand, one at a time, FIFO by validation):
      integrating → rebase onto main → re-validate → squash-land on main
      → run integrated (result_commit = landed commit), task completed
      → worktree and branch removed; history kept at refs/dagq/runs/<run-id>
    conflict / failed re-validation → needs_session
      → the supervisor resumes the session in the worktree (up to MAX_RESUME_ATTEMPTS); it resolves,
        reruns verification, rewrites the receipt → the supervisor lands the run whose
        integrate was called, or validates and reviews it with the resumed session open
        (failed receipt → run failed)
  → dependents become candidates; the resident loop claims them from the landed main
failed / interrupted run (a dead run nobody leases is recovered to interrupted first;
a run whose resumes are used up is failed with its resume_exhausted alert)
  → headless recovery job (Claude, or Codex read-only when
    [roles.recovery] says provider = "codex") → verdict; the runtime checks the action again,
    then closes its workspaces
      retry          → task ready → a new run (only without commits of its own)
      retry_inherit  → task ready → a new run carrying the branch over (once per task)
      resume         → needs_session → resumed like above (while resumes are left)
      wait           → the job runs again after recheck_after_secs
      escalate / low confidence / refused / jobs used up → decide ask
                       (retry / resume / cancel + the job's options), applied once answered;
                       a job's option goes back to the job
      job failed → triage_failed (triage by hand, read as recover by hand);
                   its provider could not be used (provider_unusable, a role that names its
                   provider) → the next round on the other provider,
                   or on the same one once its hold ends ([provider_fallback] jobs = false)
live worker alert (stalled, idle_process)
  → the same recovery job in the session's slot → repair applied, or the alert's ask;
    a failed job → the alert's ask too (reason_category recovery_failed, ADR-t609-1);
    its provider could not be used (provider_unusable, a role that names its provider)
    → the provider is held, no ask, and the alert's next job on the other provider
      (or on the same one once its hold ends with [provider_fallback] jobs = false)
```

各節は`supervisor-lifecycle/`の下の別のファイルにある。
下の見出しは各ファイルへの目次で、見出しのanchorへのリンクもここに届く。

## Roles

- [Roles](supervisor-lifecycle/roles.md)

## `up` / `down`

- [`up` / `down`](supervisor-lifecycle/up-down.md)

### `plan` / `planners`

- [`plan` / `planners`](supervisor-lifecycle/plan-planners.md)

### Build identifier

- [Build identifier](supervisor-lifecycle/build-identifier.md)

## `supervise`

- [`supervise`](supervisor-lifecycle/supervise.md)

### Handoff

- [Handoff](supervisor-lifecycle/handoff.md)

### headless jobのプロセス（記録と引き継ぎ）

- [Headless job processes](supervisor-lifecycle/headless-job-processes.md)

### claimを控える（load average）

- [claimを控える（load average）](supervisor-lifecycle/claim-hold.md)

### claimを控える（衝突の多いファイル）

- [claimを控える（衝突の多いファイル）](supervisor-lifecycle/claim-defer.md)（[ADR-0080](../adr/0080-supervisor-rereads-conflicts-config.md)）

### 空き容量を確かめる（claimと着地の検証の前）

- [空き容量を確かめる](supervisor-lifecycle/disk-space.md)（[ADR-0047](../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定44）

### 認証と利用上限のaskの待ちとanswer

- [認証と利用上限のaskの待ちとanswer](supervisor-lifecycle/queue-hold.md)（[ADR-0047](../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定42）

### 人の答えを待つrun（slotの外の待ち）

- [人の答えを待つrun（slotの外の待ち）](supervisor-lifecycle/waiting.md)

### `install`

- [`install`](supervisor-lifecycle/install.md)

### Auto-update

- [Auto-update](supervisor-lifecycle/auto-update.md)
- [Release update](supervisor-lifecycle/release-update.md)（外部のprojectのリリースのバイナリを、crates.ioの新しいリリースの検知とinboxのaskで入れ替え、pluginも揃える）

### Source repository

- [Source repository](supervisor-lifecycle/source-repository.md)（dagqのソースのrepositoryでだけ動く機能と判定）
- [Language](supervisor-lifecycle/language.md)（runtimeの固定の文字列は英語、AIが人に向けて書く文の言語の`[language]`の設定とpromptへの渡し方）
- [Landing branch](supervisor-lifecycle/landing-branch.md)（着地先のbranchとpushのremoteの`dagq.toml`の`[repository]`と、指定が無いときの推定）

### 人へのaskの通知（`[push]`）

- [人へのaskの通知（`[push]`）](supervisor-lifecycle/ask-push.md)

### Run environment

- [Run environment](supervisor-lifecycle/run-environment.md)
- [Worker model](supervisor-lifecycle/worker-model.md)（workerのmodel / effortの明示と記録、`[worker.trial]`の限定の試し、taskに由来する失敗の後の段上げ）
- [Actor model](supervisor-lifecycle/actor-model.md)（worker以外のアクターの`[roles.<role>]`、差し戻しで開き直すplannerの段上げ、`launch`の記録）

### 閾値（`[stall]`・`[conflicts]`）

- 値と意味は定義のそばのdoc comment（`domain::stall::StallConfig`、`domain::stats::conflicts::ConflictConfig`）が持つ。
- supervisorは`[stall]`を起動時に1回だけ読み、`[conflicts]`はpassごとに読み直す。

### Landing recheck

- [Landing recheck](supervisor-lifecycle/landing-recheck.md)

### Prompt

- [Prompt](supervisor-lifecycle/prompt.md)

### 1 runの異常（abandon）

- [1 runの異常（abandon）](supervisor-lifecycle/abandon.md)

## Observer

- [Observer](supervisor-lifecycle/observer.md)

## スループットの見直し

- [スループットの見直し（`throughput-review`）](supervisor-lifecycle/throughput-review.md)（supervisorがtimerで毎時・日次・週次に読むだけのheadlessのjobを始め、結果を`<queue dir>/reports/reviews/`に残し、日次・週次と規則に当たった毎時をinboxに知らせるだけのattention（`report the review`）で届ける。週次の次の一手はproposalを求めるfindingになる。[ADR-t996-1](../adr/2026-09-29-t996-1-supervisor-runs-throughput-review-jobs-and-reports-to-inbox.md)）

## `session` wrapper

- [`session` wrapper](supervisor-lifecycle/session-wrapper.md)
- [非対話のworker](supervisor-lifecycle/headless-worker.md)（1 turnを1回の非対話の呼び出しにする経路、`turns/`の依頼、turnの記録と停止、促しと復旧job）

## Receipt and session exit

- [Receipt and session exit](supervisor-lifecycle/receipt-and-session-exit.md)

### receiptの無いidleの検知

- [receiptの無いidleの検知](supervisor-lifecycle/idle-without-receipt.md)

### 生きているsessionの復旧job

- [生きているsessionの復旧job](supervisor-lifecycle/background-recovery-job.md)（stalled、非対話の idle_process）

## Validation

- [Validation](supervisor-lifecycle/validation.md)

## Review (supervisor)

- [Review (supervisor)](supervisor-lifecycle/review.md)
- [プログラムのreview](supervisor-lifecycle/review-programs.md)
- [reviewの設定の生成（`setup-review`）](supervisor-lifecycle/setup-review.md)
- [着地の前のe2e](supervisor-lifecycle/landing-e2e.md)

## Triage (supervisor)

- [Triage (supervisor)](supervisor-lifecycle/triage.md)（終わったrunの復旧job）

## Draft planners (supervisor)

- [Draft planners (supervisor)](supervisor-lifecycle/draft-planners.md)

## Finding planners (supervisor)

- [Finding planners (supervisor)](supervisor-lifecycle/finding-planners.md)（proposalを求める印の付いたfindingとaskの`propose`のanswerからplannerを立てる）
- [CI watch](supervisor-lifecycle/ci-watch.md)（設定で有効にしたrepositoryで、supervisorが着地先のbranchのCIを`gh`で見張り、既に落ちているtestの一覧を持ち、赤で`ci_failure`のfindingを記録する）

## Plan review (supervisor)

- [Plan review (supervisor)](supervisor-lifecycle/plan-review.md)

## Goal review (supervisor)

- [Goal review (supervisor)](supervisor-lifecycle/goal-review.md)（所属taskがすべて終わったgoalをacceptanceと照合し、achievedで閉じるか、gapをdraftにするか、人に聞く）

## `integrate`

- [`integrate`](supervisor-lifecycle/integrate.md)

### `needs_session`

- [`needs_session`](supervisor-lifecycle/needs-session.md)

### errorと復旧

- [errorと復旧](supervisor-lifecycle/integrate-errors.md)

## Cleanup and recovery

- [Cleanup and recovery](supervisor-lifecycle/cleanup-and-recovery.md)

### Run worktrees

- [Run worktrees](supervisor-lifecycle/run-worktrees.md)

### backendの呼び出しの失敗

- [backendの呼び出しの失敗](supervisor-lifecycle/backend-call-failures.md)

### `ask` / `answer` / `asks`

- [`ask` / `answer` / `asks`](supervisor-lifecycle/ask.md)

### コマンドとlog

- [コマンドをまたぐ約束](supervisor-lifecycle/commands.md)（計測の`stats`・`kpi`・`mark`・`marks`・`report`・`timeline`と、`status`・`events`・`watch`・`doctor`・`review`・processのlogをまたぐ約束と、各コマンドのコードの入口）
- [hostの負荷の連続の記録](supervisor-lifecycle/host-metrics.md)（supervisorがload average・プロセスの種類ごとのCPUとメモリ・メモリ・swap・pageout・ディスクの空きをqueue.dbのNodeSampleStoreと人が読む写しの`<queue dir>/host/metrics-YYYYMMDD.csv`に書き、保持の日数を過ぎた行とファイルを消す。`stats`の`host`が窓の要約を出す。外部のsamplerを止める手順）
  保持の期間のCSVは一度だけNodeSampleStoreに取り込む。
- [KPIのpush](supervisor-lifecycle/push.md)（レポートの後に目標割れの始まりと解消を記録し、host.tomlの`[push]`のコマンドのstdinに日次・週次のまとめと目標割れの即時通知を渡す。失敗の再試行とinboxのattention。ADR-0051の決定18・22・23）

### `recover` / `rebind` / `graph`の依存図

- [コマンドをまたぐ約束](supervisor-lifecycle/commands.md#復旧と束縛と依存図のコマンドをまたぐ約束)（supervisorの自動の回収と人の`recover`の分担、束縛の検査を通らない`rebind`、依存図とレポートの共有）
