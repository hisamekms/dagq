---
id: design-supervisor-lifecycle-waiting
type: design
title: "人の答えを待つrun（slotの外の待ち）"
status: current
created: 2026-09-26
updated: 2026-10-05 # task 1591: the landing queue out of the slots for light changes; task 1437; task 1440: the reopen starts a background wrapper, no workspace
last_verified: 2026-10-05 # task 1591; task 1437; task 1440
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-status
  - design-supervisor-lifecycle-stats
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

[ADR-0062](../../adr/0062-runs-waiting-for-a-person-leave-the-slot.md)の実装（決定は[ADR-0071](../../adr/0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)が置き換えて引き継いだ）。生きているsessionを持つrunが人の答えか人の操作だけを待っているあいだ、そのrunを`--parallel`のslotから外し（**待ち**）、空いたslotで他のtaskをclaimする。use caseは`src/application/supervise/waiting.rs`、イベントからの状態の導出と`stats`の集計は`src/domain/waiting.rs`。

着地の順番を待つだけのrun（reviewとe2eを終え、他のrunの着地が終わるのだけを待つ`awaiting_integration`のrun）も、`parallel`件まではslotの外に数えるもう1つの待ちで、それが空けた枠は`[supervisor] light_changes`の軽いtaskだけが使う（[ADR-t1591-1](../../adr/2026-10-04-t1591-1-landing-queue-leaves-room-for-light-changes.md)。判定は[claimを控える](claim-hold.md#着地待ちが空けた軽い枠)）。人の待ちとは別のもので、印は`Slot::landing_turn`、`used_slots()`には今までどおり含め（resume・戻り・重いtaskのclaimの判定は変わらない）、`--max-waiting`にも`waiting.count`にも数えない（sessionを閉じてから着地を待つので、上限の根拠のsessionのメモリを使わない）。人の待ちから戻り待ちのrunがslotの空きを待つあいだは、軽い枠でも新しいclaimをしない。

## 待ちの出入り

- **印**: `Slot`は`waiting: Option<Waiting>`を持つ。`Waiting`は待ちが持つaskのIDとkind、始めた時刻（queueの時計）、sessionが動いたかを比べる基準の時刻（最初のaskを開いた秒の次の秒と待ちを始めた時刻の早い方。askの時刻は秒なので、askと同じ秒のmarkerは古いとみなす。run filesの時計）、終わった時刻と理由（戻り待ち）。runのstatusとphase（`SessionWatch` / `ExitWatch` / `ReviseWatch` / `ResumeWatch`）は変えないので、戻ればphaseの続きから進む（`Revise` / `Resume`の段の計時だけは戻るときにやり直す。下の「段の計時」）。`used_slots()`は印の無いslotの数（着地中と着地の順番待ちのrunも数える）で、`fill_slots`・adopt・landingの答え・resume・triage・claimの空きの判定はこれを使う（軽い枠の判定だけが着地の順番待ちを除く）。`stats`の`idle_slots`も同じ集合で数え、`status`は同じ集合を`slots.used`と`slots.landing_queue`に分けて出す（[ADR-t610-1](../../adr/2026-09-27-t610-1-landing-runs-fill-the-slot-in-status-and-stats.md)、ADR-t1591-1）。待ちのrunも`slots`に居るので、ループはそれが残っていれば続く（drainも待つ）。
- **始める**（`start_waits`、tickの先頭。`tick(true)`の引き継ぎ待ちでは行わない）: 印の無いslotのうち、phaseが`Session`で終了を依頼しておらずwrapperが黙っておらず復旧jobが走っていないもの、`Revise`でwrapperが黙っておらず（`live.silent`）復旧jobが走っていないもの、または`Resume`で終了を依頼しておらず（`exit_requested`が無い）wrapperが黙っておらず復旧jobが走っていないもので、sessionが生きていて（wrapperが`exited_at`を記録しておらず死んでいない）、queue_holdに入っておらず、ADR-0071の決定1の表のkindのうち`worker_question`と`stalled`（`domain::waiting::waits_for`。`Session`: `worker_question` / `stalled`、`Revise` / `Resume`: `worker_question`。表の`answer_prompt`と`Exiting`の`stuck_exit`はtask 1437で対話のworkerとともに廃止し（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）、runtimeはもう開かない）の未回答でcloseされていないaskを持ち、そのaskが終わった待ちのものでない（`consumed`）run。`Revise`の`worker_question`は依頼の送信時刻（`live.asks_from`）以後に作られ、書き直したreceiptで越えていない（`ReviseWatch::holds_questions_from`、task 583）ものだけを数え（`Slot::follows`。途中で加えるask（`add_waiting_asks`）も同じ）、それより前のaskはinboxの手の配送に任せて待ちに入れない（task 582、[Review](review.md#review-supervisor)の8）。askのIDの古い順に、slotの外のrun（待ちと戻り待ち。どちらもsessionを開いたまま）の数が`--max-waiting`未満のあいだ`run_waiting_started`を記録して待ちにする。上限に達していれば`run_waiting_deferred`をaskごとに1回だけ記録し、slotに居たまま今のphaseで進む（上限が空けば次のtickで待ちに移る）。`--max-waiting 0`は待ちを使わない。
- **見張り**（`watch_waiting`。待ちのslotでは`step`を呼ばない）: sessionに何も送らず、runのstatusも変えない。
  1. leaseが自分のtokenでなければ、他のslotと同じく退く（DBには書かない。引き継いだ側がイベントから待ちを組み立てる）。
  2. wrapperが終了を記録したか、heartbeatが切れてpidも死んでいれば、非対話（`headless`）の`Session`の待ちで、receiptが無く閉じていない`worker_question`か`stalled`のaskがあるrunは、先にruntimeがsessionをbackgroundのwrapperで開き直し（runはworkspaceを開かない。[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）、開き直したか、次の試みか失ったwrapperのturnの終わり（turnの上限まで）を待つあいだは待ちを続ける（task 1372。条件・上限・間隔は[非対話のworker](headless-worker.md#待ちの最中に失ったsessionの開き直し)）。開き直さないとき（条件に当たらない、上限を超えた）は、過去に開いた`stuck_exit`のaskが残っていればその場で閉じ（`Session`なら`stalled`も`ended`で閉じる）、`session_exited`で終える（`Revise` / `Resume`はslotに戻ったtickで段が今のとおりsessionの終わりを扱う。開き直しがprocessの行を消した後に上限を超えた`Session`は、slotの`SessionWatch`が失ったwrapperのexit codeで`finish_lost_session`を呼んで`failed`にする）。`worker_question`のaskは閉じない。wrapperが黙っていれば（pidは生きている）`wrapper_heartbeat_expired`を記録し、slotで終了を依頼するか段を終えるため`wrapper_silent`で終える。`Revise`の沈黙はその後の`ExitWatch`に引き継ぎ、`wrapper_heartbeat_expired`を2度記録しない。slotに戻ったときheartbeatが戻っていれば（終了を依頼する前）、段は黙った印を消して続き、その後のaskで再び待ちに入れ、次の沈黙は`wrapper_heartbeat_expired`を改めて記録する（task 606。[wrapperが黙ったsession](silent-wrapper.md)）。
  3. runがqueue_holdに入っていれば`queue_hold`で終える。
  4. 待ちのあいだに開いた表のaskを`run_waiting_ask_added`で足す。
  5. `Session`のrunで、待ちのあいだに`SessionWatch`がまだ見ていないreceiptが現れたら`session_moved`で終える（どのkindの待ちでも。askの後始末は戻ったslotの`SessionWatch`が行う）。持っている`worker_question`が回答されたか閉じられたら`answered`で終える（答えの送信はslotに戻ったtickの`deliver_answers`）。非対話（`headless`）のrunでは、待ちが`stalled`のaskを持つあいだ、runの閉じていない`stalled`のaskに、supervisorが届ける答え（`stop`か指示の文。`wait`・`propose`・`intervene`でないもの、`stall::headless_delivers`）が付いていれば、`answered`で終える。待ちが持つaskに限らないのは、待ちのあいだに`intervene`の答えで開き直したaskが、待ちに加わる前（次のpassの`add_waiting_asks`は答えの付いたaskを加えない）に答えられることがあるため（task 1179）。非対話のsessionは自分でturnを取らず下の7で動かないので、答えで終えないと戻れない。答えはslotに戻ったtickの`StallWatch`が送る（task 1104）。`Revise`の待ちが`worker_question`を持ち、依頼の送信時刻（`ReviseWatch::sent_at`）より後にreceiptが書き直されていれば、`session_moved`で終えてその場でslotに戻す（段は書き直したreceiptの後のidleを質問のcloseを待たずに判定する。task 583、[ADR-t583-1](../../adr/2026-09-28-t583-1-revise-judges-a-rewritten-receipt-past-an-open-question.md)）。それ以外で`Revise` / `Resume`の待ちが`worker_question`を持つあいだは、以下の6・7で終えない（ほかの理由で戻しても、段は閉じていない`worker_question`のあいだ書き直さずのidleでも時間切れでも終わらず、そのaskはもう待ちに入れないので、答えまでslotを塞ぐ。ADR-0071の決定2・16）。
  6. `stalled`を持つ`Session`のrunでreceiptが無ければ、`StallWatch::poll_quiet`でそのaskを追う（`wait`の答えでaskを閉じて計時をやり直し、閾値を過ぎたら次の`stalled`のaskを開く。促しは送らない）。
  7. `stalled`を持つrunで、idle markerかreceiptが基準の時刻より新しければ`session_moved`で終える。
- **終わり**（`end_wait`）: `run_waiting_ended`（`ask_id`、`ask_kind`、`cause`、`waited_secs`）を記録し、持っていたaskを`consumed`に足す。`answered`と`session_exited`は戻り待ちになり、それ以外（`session_moved` / `queue_hold` / `wrapper_silent` / `phase_changed`。過去の記録の`dialog_cleared`も）はその場でslotに戻す（`run_slot_regained`の`over_parallel`は戻った後のslotの数が`--parallel`を超えたか）。
- **戻す**（`return_waiting_runs`）: `drive`のループの毎回、引き継ぎの判定と`fill_slots`より前に（claimを止めていても、drain中も）、戻り待ちのrunを終わった順に、`used_slots()`が`--parallel`未満のあいだslotへ戻し、`run_slot_regained`（`slot_wait_secs`、`over_parallel`）を記録する。戻ったrunは今のphaseのとおり進む（`worker_question`なら答えを次のturnの依頼として届ける）。


## 差し戻しと解消依頼の段（`Revise` / `Resume`）

ADR-0071の決定15〜18。

- **計時の再開**: slot の外では timeout を止め、戻った時点で restart_clocks を呼ぶ。回答と続行の依頼を file に書いた時刻を新しい起点にする。stale receipt の書き直しは元の依頼の時刻で比較する。対話 worker の入力欄待ちの時計は廃止した。
- **`worker_question`のidle**（決定16）: `ReviseWatch`（task 238）と同じく、`ResumeWatch`もcloseされていない`worker_question`があるあいだは、idleでも段を終えず、`resume_timeout`も数えず、古いreceiptの書き直しも頼まない（`has_unclosed_worker_question`の判定がそれらより前にある）。ただし`ReviseWatch`は、依頼より後に書き直したreceiptの後のidleだけは質問のcloseを待たずに判定する（task 583、ADR-t583-1）。上限でslotに居るとき（`run_waiting_deferred`）と`--max-waiting 0`でも同じ。答えを送ったときはその時刻を、手で配送されてaskがcloseされたとき（closeの秒が最後の入力の秒より後）はcloseの時刻を最後の入力（`live.input_at`）にし、段の計時をそこから数え直す。以後のidleはそれより新しいmarkerだけを数える。
- **Resume の配送**: ResumeWatch の live は SessionWatch を持つ。解消依頼から終了の依頼まで deliver_answers を回し、回答は次の turn に書く。request_exit は exit_requested を記録して終了の依頼を書く。session_takes_answers は resume 中で終了要求前の needs_session も配送対象にする。ダイアログの監視・入力欄待ち・exit_typed の判断は廃止した。
- **phase**: `run_waiting_started`の`phase`と`status`の`waiting[].phase`は`revise` / `resume`。`status`はrunの今のstatus（`awaiting_integration` / `needs_session`）。`stats`の`waiting`は`ask_kind`ごとに数え、phaseでは分けない（決定18）。workerの経路（`interactive` / `headless`）では`by_route`で分ける（task 1370、[`stats`](stats.md)）。
- **引き継ぎとadopt**: `Revise`はadoptと引き継ぎのどちらでもイベント（`review_anchor`）から組み立て、`Resume`は引き継ぎの`handoff.json`から、それが無ければadoptと同じくイベントから組み立てる（task 356・640）。どちらも組み立てた後の`restore_waiting`で待ちを戻す。

ADR-0062の決定2の`cause`に、この実装は`wrapper_silent`（`Session`でwrapperが黙った。slotで終了を依頼する）と`phase_changed`（引き継ぎやadoptで待てないphaseに組み立て直した、またはadoptで上限を超えた）を足している。`lease_lost`と`run_ended`は書かない（leaseを失ったプロセスは書かない。adoptした側がイベントから同じ待ちを続けるので、書くとその待ちを消してしまう）。代わりに`WaitState::of`は、`run_waiting_started`の後に`lease_acquired` / `lease_released` / `run_recovered` / `runtime_error`があれば待ちは無いとみなす（待ちを持っていたsupervisorがrunを失った。adoptと引き継ぎはこれらを書かない）。

## 引き継ぎとadopt

- **組み立て直し**（`restore_waiting`）: execの引き継ぎ（[Handoff](handoff.md)）とadopt（[`supervise`](supervise.md)の5）でslotを組み立てた後、runのイベントから`consumed`（終わった待ちのask）、`deferred`、今の待ち（`WaitState::of`: 最新の`run_waiting_started`の後に`run_waiting_ended`が無ければ待ち、`run_waiting_ended`の後に`run_slot_regained`が無ければ戻り待ち）を戻す。待ちに入った時刻はイベントから取り、`run_waiting_started`を記録し直さない。組み立て直したphaseが待てないもの（例えばreviewからやり直すrun）なら`phase_changed`で終えてslotに戻す。引き継ぎは上限を超えていても待ちのまま戻す（上限を下回るまで新しい待ちを入れない）。
- **adopt**: `adopt_stale_runs`は空きslotの判定を先頭の打ち切りではなくrunごとに行い、イベントの上で待っているrunは待ちの数が上限未満ならslotの空きを要さずに待ちとして引き継ぐ。上限に空きが無ければ今のとおりslotの空きを待って引き継ぎ、待ちを`phase_changed`で終えてslotのrunとして扱う（そのaskでまた待ちに入れる）。

## 登録と見せ方

- `supervise --max-waiting N`（0で待ちを使わない。明示が無ければ`dagq.toml`の`[supervisor] max_waiting`、それも無ければ4。[Run environment](run-environment.md)の`[supervisor]`、task 698）と`up --max-waiting N`（明示されたときだけ`supervise`の引数に足す）。supervisorは登録（と引き継ぎの取り戻し、`[supervisor]`の読み直しで変わったとき）に`supervisors.max_waiting`（schema v37、互換の列。[persistence](../persistence.md)）と出どころの`max_waiting_source`を書く。
- `status`の各登録に`slots: {used, landing_queue, parallel, source}`（`landing_queue`は着地の順番待ち）と`waiting: {count, returning, limit, source}`（`source`は値の出どころ）（`count`は待ちと戻り待ちの合計で`--max-waiting`が数えるものと同じ、`returning`はその内訳の戻り待ち。数え方は`src/domain/waiting.rs`の`WaitCount`の1つで、supervisorの`waiting_runs()`と`status`の両方が使う）、全体に`waiting`の配列（[`status`](status.md)）。`stats`に`waiting`（[`stats`](stats.md)。窓はrunのpageではなく求めた窓の全体で、経路ごとの`by_route`を持つ。task 1370）。

## test

`tests/it/runtime_headless_reopen.rs` は wrapper の開き直し、`runtime_waiting` と `runtime_waiting_stages` は非対話の質問・slot・上限・adopt の待ちを確かめる。対話だけの画面と終了待ちの test は撤去した。`domain::waiting` と stats の unit test は過去を含む待ちの記録と集計を確かめる。
