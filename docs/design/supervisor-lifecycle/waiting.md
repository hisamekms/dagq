---
id: design-supervisor-lifecycle-waiting
type: design
title: "人の答えを待つrun（slotの外の待ち）"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-commands
  - design-supervisor-lifecycle-handoff
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-needs-session
  - adr-0062
  - adr-0071
  - adr-0039
  - adr-0045
  - design-persistence
  - adr-t610-1
  - adr-t1591-1
  - design-supervisor-lifecycle-headless-worker
---

# 人の答えを待つrun（slotの外の待ち）

生きているsessionを持つrunが人の答えか人の操作だけを待っているあいだ、そのrunを並列の上限のslotから外し（**待ち**）、空いたslotで他のtaskをclaimする。
待ちの約束は[ADR-0071](../../adr/0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)、最初の決定は[ADR-0062](../../adr/0062-runs-waiting-for-a-person-leave-the-slot.md)を読む。
待ちの実行の入口は `src/application/supervise/execution/waiting.rs`、イベントからの状態の導出と集計の入口は `src/domain/waiting.rs`。

着地の順番待ちは人の待ちとは別で、並列の上限の件数まで空けた枠は軽いtaskだけが使う（[ADR-t1591-1](../../adr/2026-10-04-t1591-1-landing-queue-leaves-room-for-light-changes.md)、[claimを控える](claim-hold.md#着地待ちが空けた軽い枠)）。
着地待ちはsessionを閉じているため人の待ちの上限にも数にも含めず、resumeや人の待ちからの戻り、重いtaskのclaimに対するslotの数え方も変えない。
人の待ちから戻るrunが空きを待つあいだは、軽い枠でも新しいclaimをしない。

## 待ちの出入り

待ちはrunのstatusを変えずにslotに付ける印で、戻ればphaseの続きから進む（印の定義は `Waiting`）。
待ちのrunもsupervisorが持ち続けるため、drainもその終了を待つ。
slotの数え方を知りたいときは `Supervisor::used_slots` を読む。
着地中と着地の順番待ちもslotに数え、statusとstatsも同じ集合を数える（[ADR-t610-1](../../adr/2026-09-27-t610-1-landing-runs-fill-the-slot-in-status-and-stats.md)）。

待ちに入れる条件と上限を超えたrunの扱いは `Supervisor::start_waits`、見張りが待ちを終える条件は `Supervisor::watch_waiting` を読む。
見張りはsessionへ送らず、答えはslotに戻った後のtickで届ける。
失ったsessionの開き直しもbackgroundで行い、workspaceを開かない（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)、[開き直しの境界](headless-worker.md#待ちの最中に失ったsessionの開き直し)）。
待ちを終えた記録と戻り先は `Supervisor::end_wait`、戻す順と計時の再開は `Supervisor::return_waiting_runs` を読む。
戻す処理は新しいclaimより先で、claimを止めたあいだもdrain中も続く。

## taskのhold

人の答えを待つrunのtaskをholdしたときの保留と解除の境界は[needs_sessionのtaskのhold](needs-session.md#taskのhold)、解除の判断と答えの持ち越し先は `domain::task_hold::release_decision` を読む（[ADR-t1879-1](../../adr/2026-10-07-t1879-1-hold-a-task-at-the-turn-boundary-and-release-it-explicitly.md)）。
同じrunをresumeする解除だけが未配送の答えを解消依頼に載せ、新しいrunにする解除では人の判断を[workerのprompt](prompt.md#workerのprompt)に引き継ぐ。
holdの待ちはprocessを持たず、人の答えの待ちの上限や見張りの対象にならない。
holdの待ちも人の答えの待ちと同じく分けて測り、除いた値と含む値を並べる（[計測](../measurement.md#区間とタグ)）。

## 差し戻しと解消依頼の段（`Revise` / `Resume`）

slotの外では段のtimeoutを止め、戻ったら計時をやり直す（`Phase::restart_stage_clocks`、ADR-0071決定15〜18）。
質問を持つ段のidleとtimeoutの扱いは `ReviseWatch` と `ResumeWatch` のdoc commentを読む。
質問の答えを待っているあいだは、上限のためslotに残っていても段を終えない。
差し戻しでは書き直したreceiptの後のidleだけは質問のcloseを待たずに判定する（[ADR-t583-1](../../adr/2026-09-28-t583-1-revise-judges-a-rewritten-receipt-past-an-open-question.md)）。
大きなcontextの後に[新しいsession](../provider-lifecycle.md#大きなcontextからの新しいsession)で続く場合も、同じrunの待ちとして扱う。

## 引き継ぎとadopt

待ちはイベントから組み立て直し、開始を記録し直さない。
復元する状態とphaseが変わったときの扱いは `Supervisor::restore_waiting`、引き継ぐphaseの読み先は `handoff_slots::Takeover` と `Supervisor::resume` を読む。
引き継ぎは上限を超えても待ちを維持し、adoptは上限に空きがあるときslotの空きを要さずに待ちを引き継ぐ（入口は `Supervisor::adopt_stale_runs`）。
leaseを失ったprocessは待ちの終わりを書かない。
adopt側が同じ待ちを続けるためで、待ちを失効させる記録の読み方は `WaitState::of` が持つ。

## 空き容量のための掃除

待ちのrunはleaseとslotを持つため、終わったrunや誰も作業していないrunの通常の掃除の候補にならない。
空き容量のための掃除では、待ちのrunもturnが終わって何も動いていないときだけビルド成果物を消す。
通常の掃除で消せるのは次のbuildが読まない古いcacheだけで、同じ排他を守る（[終わっていないrunのビルド成果物](run-worktrees.md#終わっていないrunのビルド成果物)）。

待ちのrunを掃除へ渡す境界は `Supervisor::waiting_worktrees`、候補の選び方は `cleanup::waiting_candidate`、消す直前の再確認は `cleanup::still_waiting` を読む。
待ちはturnの終わりより先に始まりうるので、待ちに入っただけでは掃除できない。
掃除jobが予約したrunでは答えの配送も次のturnもsessionの開き直しも始めず、jobが通り過ぎてから通常の順で答えを届ける。
ソース・未commitの変更・receipt・runの記録・wrapperの実行場所は残し、次のturnはsccacheでビルドし直せる。
workerのpromptには掃除を通知しない。
ビルド成果物はいつでも作り直せるため、workerの手順が変わらないことが約束である。
着地slotを待つrunの掃除は[Run worktrees](run-worktrees.md#終わっていないrunのビルド成果物)を読む。

## 登録と見せ方

待ちの上限の設定の入口は[Run environment](run-environment.md)、表示の入口は[commands](commands.md)。
待ちの数をsupervisorとstatusで共通に数える定義は `domain::waiting::WaitCount` を読む。
待ちの計測はaskのkindとworkerの経路で分け、phaseでは分けない（ADR-0071決定18）。

## test

wrapperの開き直しや引き継ぎをまたぐ境界はintegration test、待ちの判断と集計はunit testで確かめる。
配線のtestの置き場所は[testの制約](../../development/testing.md)の「判断と境界のtest」に従い、既存の配線のintegration testは移し替えまで残す。
