---
id: design-supervisor-lifecycle-queue-hold
type: design
title: "認証と利用上限のaskの待ちとanswer"
status: current
created: 2026-09-27
scope: runtime
related:
  - adr-0047
  - adr-t813-2
  - adr-t1063-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-ask
  - design-supervisor-lifecycle-claim-hold
  - design-supervisor-lifecycle-disk-space
  - design-supervisor-lifecycle-waiting
  - design-supervisor-lifecycle-commands
---

# 認証と利用上限のaskの待ちとanswer

## 目的

Claude Codeのログインが切れるか利用上限に達すると、新しいrunもheadlessのjobも同じ壁で止まる。
runtimeはそれをqueueで1件の`queue_hold`のask（`authentication`、または`cost`で`subject: usage_limit`）にまとめ、openな間はClaudeで動く新しいclaimとheadless jobを控え、人のanswerを自分で適用する（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定42の「待ち」と「answer」）。
ディスクの`cost`のaskは[空き容量を確かめる](disk-space.md)が扱い、ここの控えとanswerの適用には入らない。

## 流れ

```text
壁の検知（workerのturn・失敗したheadless job・runtimeのplannerのturn）
  └─ askを開くか加わる（runとjobをaffectedに足す）
       └─ openな間: claimとClaudeのheadless jobを控える、走っているrunはleaseを持ったまま
            └─ 人のanswer
                 ├─ done: 各supervisorが自分のslotのrunに印を書き続きの文を予約する → askを閉じる → 失敗したjobを起動し直す
                 │    └─ 予約した文はその後にsessionの監視が送り、askを閉じるのは送るのを待たない
                 └─ cancel_affected: 各supervisorが自分のslotのrunを手放す → askを閉じる
```

## 責務と境界

- 判断（どのaskが控えるか、壁の種類、jobの項目、answerを適用するか）は`src/domain/queue_hold.rs`が持つ。
- 控えの読み直し・answerの適用・askを閉じること・jobの起動し直しは`src/application/supervise/queue_hold.rs`が持ち、module docが範囲を持つ。
- askを開く・加わるのは`application::ask::hold`で、壁の文言は`infrastructure::claude`の1か所で読む。
- 控えの単位はproviderごとで、このaskはClaudeの控えである（[ADR-t813-2](../../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)の決定6）。
  Codexの控えはaskを開かない`ProviderHold`で、[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)が持つ。

## 不変条件

- 理由と`subject`ごとにopenな控えのaskは1件で、後から止まったrunとjobはそこに加わる（[ask](ask.md)）。
- answerは1つのrunに1回だけ適用される（`hold_answer_applied`の印）。
- askが閉じるまで、askに居るrunへ促しとstalledのaskは打たれない。
- 走っているrunはleaseとsessionを持ったまま進み、着地（`integrate`）は控えない。

## 検知

- 壁の型と、壁ごとのaskの理由・`subject`・event・questionは`domain::queue_hold::Wall`。
- workerのturnの壁はproviderが読み、切り替えか控えかは`domain::provider_switch::wall_move`が決める（[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)）。
- 失敗したheadless jobの出力の分類は`JobDesk::job_failure`、askへの加わりは`JobDesk::raise_job_wall`で、控えはそのpassから効く。
- askの`affected`のjobの項目は`domain::queue_hold::HoldJob`で、runのIDとの区別は`is_run_entry`。
- 落とし穴: Codexの失敗はこのaskに入れない（[Observer](observer.md#codexで動かす)・[Triage](triage.md#codexで動かす)）。
- runtimeのplannerのturnが壁で止まったときのaskと待ちは[`plan` / `planners`](plan-planners.md#runtimeのplannerの経路)の「使えないとき」が持つ。

## 控え

- 毎passの読み直しは`Supervisor::check_queue_hold`で、控えは`Supervisor.queue_hold`に持つ。
- どのaskが控えるかは`domain::queue_hold::reason_of`と`hold_of`。
- claimの判定は`ClaimHold::judge`で、控えはディスクとloadより先に判定する（[claimを控える](claim-hold.md)）。
  控えがclaimを止めるのは、どのworkerにも経路が無いときだけである（`Supervisor::routes`）。
- jobの行き先は`Supervisor::start_route`（復旧job・observer・スループットの見直し）、runのreviewは`review_route`が控えを読み、plan reviewとgoal reviewは`JobDesk::held`を読む。
  `provider`を書いた役割のjobは控えの間も使えるproviderで起動する（[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)の決定5）。
  切り替えを止める設定の扱いは[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)と[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)。
- reviewの待ちは`Phase::ReviewHeld`で、プログラムのreviewは控えの間も流れる（[プログラムのreview](review-programs.md)）。
  始まった後のjobが止まったときは、そのjobだけが他のjobの答えを保ってreviewの中で待つ（[Review](review.md#reviewのprovider)）。
- `--no-claude`のsupervisorは控えを持たず、reviewは`provider`を書いて使えるCodexで動くjobなら起動し、`provider`が無いかCodexを使えなければ人の`approve_landing`に回る（`review_route`、[ADR-t1207-1](../../adr/2026-09-30-t1207-1-codex-run-review.md)の決定4）。
- askに居るrunの待ちは`queue_hold`で戻る（[人の答えを待つrun](waiting.md)）。

## answerの適用

- askのoptionsのどれかの答えだけをruntimeが適用し（`domain::queue_hold::applies`）、他の答えはinboxが人に見せる。
- runへの適用は`Supervisor::apply_hold_answer`、段ごとの結果は`hold_outcome`。
  各supervisorは自分のslotのrunにだけ適用する。
- 落とし穴: 印（`hold_answer_applied`）は答えが働く前に書く。
  手放したrunはleaseを持たないので、先に手放すと閉じる側のsupervisorがそのrunを`unwatched`と読む。
- `done`の続きの文は`SessionWatch::continue_after_hold`が送り、文面は`queue_hold::CONTINUE_TEXT`。
- `cancel_affected`は`Supervisor::hold_canceled`で`abandon`と同じく手放し、成果を捨てるかは人が`recover`で決める。
- 引き継ぎ（exec）の後のsupervisorは、runのslotを組み立て直すまで答えを適用しない（`check_queue_hold`の`apply`）。

## askを閉じる

- 閉じる判定は`Supervisor::close_hold`で、結果の分類は`queue_hold_applied`（`domain::queue_hold::QUEUE_HOLD_APPLIED`）のdoc commentが持つ。
- 別の生きているsupervisorがleaseを持つrunは、そのsupervisorの次のpassが適用するのを、答えから待つ時間（`APPLY_WAIT_SECS`）まで待つ。
- 閉じたsupervisorだけが`done`で失敗したjobを起動し直し（`Supervisor::restart_failed_jobs`）、Codexの控えも解く（ADR-t813-2の決定6）。
- 落とし穴: 回答済みのaskは控えないので、claimとheadless jobはaskが閉じるのを待たず答えた時点で再開する。
- `elsewhere`・`unwatched`のrunには答えが適用されていないので、後の手当ては`close_hold`のdoc commentのとおりinboxかstallの促しが行う。
