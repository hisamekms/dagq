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
戻りと新しいclaimの判断の入口は `src/domain/admission.rs`。

着地の列は人の待ちとは別で、workerのslotとの境界は[claimを控える](claim-hold.md#workerのslotと着地の列)を読む。
着地の列はsessionを閉じているため人の待ちの上限にも数にも含めない。
人の待ちから戻れるrunがworkerのslotの空きを待つあいだは、新しいclaimをしない。
claimを控える判断では、待ちが終わって空きslotを待つrunのうち、Admissionが「今進めるrun」と判定するものを戻り待ちに数える。
「今進めるrun」はholdも答え待ちもなく、その段階に効くCI・env・landing branchの停止もないrunを指す（[ADR-t5292-1](../../adr/2026-11-18-t5292-1-admission-unifies-starts-resources-and-reasons.md)決定2）。
人の答えを待ち続けているrunとhold中のrunはこの数から外れ、戻り待ちのrunに残る未回答のaskやCI・env・landing branchの停止は戻りを妨げない。

## 待ちの出入り

待ちはrunのstatusを変えずにslotに付ける印で、戻ればphaseの続きから進む。
workerのslotは人の待ちと着地の列を除き、着地の順番待ちのうち上限を超えた分だけを含む。
戻り待ちはこのworkerのslotの空きを待つ。

待ちの最中は答えをsessionへ届けず、slotに戻ってから届ける。
失ったsessionの開き直しもbackgroundで行い、workspaceを開かない（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)、[開き直しの境界](headless-worker.md#待ちの最中に失ったsessionの開き直し)）。

答えが届くなどして待ちが終わり、workerの空きslotを待つrunは、hold中はslotの外に残って未使用のディスク予約を持たず、holdを外すまでslotを取り戻さない。
claimを止めたあいだも戻りは進み、holdされていないrunは答えが届いて待ちが終わればdrain中も空いたslotに戻れる。
停止要求を受けたdrainは進行中の工程を終え、待ちのrunでは今進めるものだけを待つ。
hold中のrunがslotの外に残っていてもdrainは終わり、holdや答え待ちで止まったrunは待ちの記録とleaseを保ったまま次のsupervisorに渡る（[ADR-t5293-1](../../adr/2026-11-19-t5293-1-drain-keeps-stopped-waits-and-landing-skips-unanswered-entries.md)決定2）。

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
hold中の待ちは上限に達していてもslotの外で引き継ぐ。
leaseを失ったprocessは待ちの終わりを書かない。
adopt側が同じ待ちを続けるためで、待ちを失効させる記録の読み方は `WaitState::of` が持つ。

## 空き容量のための掃除

待ちのrunのビルド成果物を消すかは、予約の方式と独立の掃除モードが決める。
既定ではclaimが空き容量で止まるときだけ、待ちに入った古い順に必要量まで消し、人の答え待ちも通常の掃除では残す。
通常の掃除で消すモードでも、leaseを持つsupervisor自身が渡し、turnが終わって何も動いていないrunだけを候補にする。
ただし、開いたaskがstalledだけの答え待ちとturnの時間上限で止まった後のresume待ちはビルド成果物を残し、どちらも古いcacheの通常の掃除には残す。
理由は[待ちの間の削除の測定](../../plans/run-incremental-removal-on-wait-spike.md#後続の実装の-task-への推奨)を読む。
古いcacheの通常の掃除はモードとは独立に続け、同じ排他を守る（[終わっていないrunのビルド成果物](run-worktrees.md#終わっていないrunのビルド成果物)）。

待ちのrunを掃除へ渡す境界は `Supervisor::waiting_worktrees`、候補の選び方は `cleanup::waiting_candidate`、消す直前の再確認は `cleanup::still_waiting` を読む。
待ちはturnの終わりより先に始まりうるので、待ちに入っただけでは掃除できない。
答え待ちと着地slot待ちはjobが届いたときだけ予約し、それまでは答えの適用やsessionの開き直しを妨げない。
先に待ちが動けば候補から外し、予約したrunだけはjobが通り過ぎてから通常の順で答えを届ける。
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
