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

[ADR-0062](../../adr/0062-runs-waiting-for-a-person-leave-the-slot.md)の実装（決定は[ADR-0071](../../adr/0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)が置き換えて引き継いだ）。
生きているsessionを持つrunが人の答えか人の操作だけを待っているあいだ、そのrunを並列の上限のslotから外し（**待ち**）、空いたslotで他のtaskをclaimする。
use caseは`src/application/supervise/waiting.rs`、イベントからの状態の導出と`stats`の集計は`src/domain/waiting.rs`。

着地の順番を待つだけのrun（reviewとe2eを終え、他のrunの着地が終わるのだけを待つ`awaiting_integration`のrun）も、並列の上限の件数まではslotの外に数えるもう1つの待ちで、それが空けた枠は`[supervisor] light_changes`の軽いtaskだけが使う（[ADR-t1591-1](../../adr/2026-10-04-t1591-1-landing-queue-leaves-room-for-light-changes.md)。判定は[claimを控える](claim-hold.md#着地待ちが空けた軽い枠)）。
人の待ちとは別のもので、印は`Slot::landing_turn`、`used_slots()`には含め（resume・戻り・重いtaskのclaimの判定は変わらない）、待ちの上限にも待ちの数にも数えない（sessionを閉じてから着地を待つので、上限の根拠のsessionのメモリを使わない）。
人の待ちから戻り待ちのrunがslotの空きを待つあいだは、軽い枠でも新しいclaimをしない。

## 待ちの出入り

- **印**: `Slot`は`waiting: Option<Waiting>`を持つ。
  sessionが動いたかを比べる基準の時刻は`Waiting::since`。
  runのstatusとphase（`SessionWatch` / `ExitWatch` / `ReviseWatch` / `ResumeWatch`）は変えないので、戻ればphaseの続きから進む（`Revise` / `Resume`の段の計時だけは戻るときにやり直す。下の「段の計時」）。
  `used_slots()`は印の無いslotの数（着地中と着地の順番待ちのrunも数える）で、slotの空きの判定はこれを使う（軽い枠の判定だけが着地の順番待ちを除く）。
  `stats`と`status`も同じ集合で数え、`status`は着地の順番待ちを分けて出す（[ADR-t610-1](../../adr/2026-09-27-t610-1-landing-runs-fill-the-slot-in-status-and-stats.md)、ADR-t1591-1）。
  待ちのrunも`slots`に居るので、ループはそれが残っていれば続く（drainも待つ）。
- **始める**（`start_waits`、tickの先頭。`tick(true)`の引き継ぎ待ちでは行わない）: 印の無いslotのうち、phaseが`Session`で終了を依頼しておらずwrapperが黙っておらず復旧jobが走っていないもの、`Revise`でwrapperが黙っておらず復旧jobが走っていないもの、または`Resume`で終了を依頼しておらずwrapperが黙っておらず復旧jobが走っていないもので、sessionが生きていて（wrapperが終了を記録しておらず死んでいない）、queue_holdに入っておらず、ADR-0071の決定1の表のkindのうち`worker_question`と`stalled`（`domain::waiting::waits_for`。`Session`: `worker_question` / `stalled`、`Revise` / `Resume`: `worker_question`）の未回答でcloseされていないaskを持ち、そのaskが終わった待ちのものでない（`consumed`）run。
  `Revise`の`worker_question`は依頼の送信時刻以後に作られ、書き直したreceiptで越えていないものだけを数え（途中で加えるaskも同じ）、それより前のaskはinboxの手の配送に任せて待ちに入れない（[Review](review.md#review-supervisor)の8）。
  askのIDの古い順に、slotの外のrun（待ちと戻り待ち。どちらもsessionを開いたまま）の数が待ちの上限未満のあいだ`run_waiting_started`を記録して待ちにする。
  上限に達していれば`run_waiting_deferred`をaskごとに1回だけ記録し、slotに居たまま今のphaseで進む（上限が空けば次のtickで待ちに移る）。
- **見張り**（`watch_waiting`。待ちのslotでは`step`を呼ばない）: sessionに何も送らず、runのstatusも変えない。
  1. leaseが自分のtokenでなければ、他のslotと同じく退く（DBには書かない。引き継いだ側がイベントから待ちを組み立てる）。
  2. wrapperが終了を記録したか、heartbeatが切れてpidも死んでいれば、非対話（`headless`）の`Session`の待ちで、receiptが無く閉じていない`worker_question`か`stalled`のaskがあるrunは、先にruntimeがsessionをbackgroundのwrapperで開き直し（runはworkspaceを開かない。[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）、開き直したか、次の試みか失ったwrapperのturnの終わり（turnの上限まで）を待つあいだは待ちを続ける（条件・上限・間隔は[非対話のworker](headless-worker.md#待ちの最中に失ったsessionの開き直し)）。
     開き直さないとき（条件に当たらない、上限を超えた）は、過去に開いた`stuck_exit`のaskが残っていればその場で閉じ（`Session`なら`stalled`も`ended`で閉じる）、`session_exited`で終える（`Revise` / `Resume`はslotに戻ったtickで段が今のとおりsessionの終わりを扱う。開き直しがprocessの行を消した後に上限を超えた`Session`は、slotの`SessionWatch`が失ったwrapperのexit codeで`finish_lost_session`を呼んで`failed`にする）。
     `worker_question`のaskは閉じない。
     wrapperが黙っていれば（pidは生きている）`wrapper_heartbeat_expired`を記録し、slotで終了を依頼するか段を終えるため`wrapper_silent`で終える。
     `Revise`の沈黙はその後の`ExitWatch`に引き継ぎ、`wrapper_heartbeat_expired`を2度記録しない。
     slotに戻ったときheartbeatが戻っていれば（終了を依頼する前）、段は黙った印を消して続き、その後のaskで再び待ちに入れ、次の沈黙は`wrapper_heartbeat_expired`を改めて記録する（[黙ったwrapper](receipt-and-session-exit.md#黙ったwrapper)）。
  3. runがqueue_holdに入っていれば`queue_hold`で終える。
  4. 待ちのあいだに開いた表のaskを`run_waiting_ask_added`で足す。
  5. `Session`のrunで、待ちのあいだに`SessionWatch`がまだ見ていないreceiptが現れたら`session_moved`で終える（どのkindの待ちでも。askの後始末は戻ったslotの`SessionWatch`が行う）。
     持っている`worker_question`が回答されたか閉じられたら`answered`で終える（答えの送信はslotに戻ったtickの`deliver_answers`）。
     非対話（`headless`）のrunでは、待ちが`stalled`のaskを持つあいだ、runの閉じていない`stalled`のaskに、supervisorが届ける答え（`stop`か指示の文。`wait`・`propose`・`intervene`でないもの）が付いていれば、`answered`で終える。
     待ちが持つaskに限らないのは、待ちのあいだに`intervene`の答えで開き直したaskが、待ちに加わる前（次のpassは答えの付いたaskを加えない）に答えられることがあるため。
     非対話のsessionは自分でturnを取らず下の7で動かないので、答えで終えないと戻れない。
     答えはslotに戻ったtickの`StallWatch`が送る。
     `Revise`の待ちが`worker_question`を持ち、依頼の送信時刻より後にreceiptが書き直されていれば、`session_moved`で終えてその場でslotに戻す（段は書き直したreceiptの後のidleを質問のcloseを待たずに判定する。[ADR-t583-1](../../adr/2026-09-28-t583-1-revise-judges-a-rewritten-receipt-past-an-open-question.md)）。
     それ以外で`Revise` / `Resume`の待ちが`worker_question`を持つあいだは、以下の6・7で終えない（ほかの理由で戻しても、段は閉じていない`worker_question`のあいだ書き直さずのidleでも時間切れでも終わらず、そのaskはもう待ちに入れないので、答えまでslotを塞ぐ。ADR-0071の決定2・16）。
  6. `stalled`を持つ`Session`のrunでreceiptが無ければ、`StallWatch::poll_quiet`でそのaskを追う（`wait`の答えでaskを閉じて計時をやり直し、閾値を過ぎたら次の`stalled`のaskを開く。促しは送らない）。
  7. `stalled`を持つrunで、idle markerかreceiptが基準の時刻より新しければ`session_moved`で終える。
- **終わり**（`end_wait`）: `run_waiting_ended`を記録し、持っていたaskを`consumed`に足す。
  `answered`と`session_exited`は戻り待ちになり、それ以外（`session_moved` / `queue_hold` / `wrapper_silent` / `phase_changed`。過去の記録の`dialog_cleared`も）はその場でslotに戻す。
- **戻す**（`return_waiting_runs`）: `drive`のループの毎回、引き継ぎの判定と`fill_slots`より前に（claimを止めていても、drain中も）、戻り待ちのrunを終わった順に、`used_slots()`が並列の上限未満のあいだslotへ戻し、`run_slot_regained`を記録する。
  戻ったrunは今のphaseのとおり進む（`worker_question`なら答えを次のturnの依頼として届ける）。

## taskのhold

人の答えを待つrunのtaskがholdされると（[ADR-t1879-1](../../adr/2026-10-07-t1879-1-hold-a-task-at-the-turn-boundary-and-release-it-explicitly.md)）、待ちの見張りの前に再計画と同じ保留がそのrunを取り、wrapperの終了を確かめて`needs_session`（`hold_parked`）にしてslotの印ごと放す（[needs_session](needs-session.md#taskのhold)）。
答えが来ても次のturnに届けず、届いていない答えは解除の後のresumeの依頼に載せる。
holdの待ちはprocessを持たないので、`--max-waiting`と`waiting.count`に数えず、この文書の見張り（stall・沈黙・開き直し）の対象にもならない。
`timeline`と`stats`・`kpi`はholdの待ち（工程`held`）を人の答えの待ちと同じく分け、除いた値と含む値を並べる（[計測](../measurement.md#区間とタグ)）。

## 差し戻しと解消依頼の段（`Revise` / `Resume`）

ADR-0071の決定15〜18。

- **計時の再開**: slot の外では timeout を止め、戻った時点で restart_clocks を呼ぶ。
  回答と続行の依頼を file に書いた時刻を新しい起点にする。
  stale receipt の書き直しは元の依頼の時刻で比較する。
- **`worker_question`のidle**（決定16）: `ReviseWatch`と同じく、`ResumeWatch`もcloseされていない`worker_question`があるあいだは、idleでも段を終えず、`resume_timeout`も数えず、古いreceiptの書き直しも頼まない（`has_unclosed_worker_question`の判定がそれらより前にある）。
  ただし`ReviseWatch`は、依頼より後に書き直したreceiptの後のidleだけは質問のcloseを待たずに判定する（ADR-t583-1）。
  上限でslotに居るとき（`run_waiting_deferred`）と上限が0のときも同じ。
  答えを送ったときはその時刻を、手で配送されてaskがcloseされたとき（closeの秒が最後の入力の秒より後）はcloseの時刻を最後の入力にし、段の計時をそこから数え直す。
  以後のidleはそれより新しいmarkerだけを数える。
- **新しいsession**: 差し戻しかresumeが大きなcontextの後に新しいsessionで続くとき（[Agent provider lifecycle](../provider-lifecycle.md#大きなcontextからの新しいsession)）も、段は同じrunのidle marker・receipt・質問を追う。
  依頼より古いreceipt・idle marker・質問は前のsessionのものとして段を終えず、待ちにも入れない。
- **Resume の配送**: ResumeWatch の live は SessionWatch を持つ。
  解消依頼から終了の依頼まで回答を配送し、回答は次の turn に書く。
  終了の依頼は exit_requested を記録して書く。
  resume 中で終了要求前の needs_session も配送対象にする。
- **phase**: `stats`の待ちはaskのkindごとに数え、phaseでは分けない（決定18）。
  workerの経路ごとには分ける（[`stats`](commands.md)）。
- **引き継ぎとadopt**: `Revise`はadoptと引き継ぎのどちらでもイベント（`review_anchor`）から組み立て、`Resume`は引き継ぎの`handoff.json`から、それが無ければadoptと同じくイベントから組み立てる。
  どちらも組み立てた後の`restore_waiting`で待ちを戻す。

ADR-0062の決定2の`cause`に、この実装は`wrapper_silent`（`Session`でwrapperが黙った。slotで終了を依頼する）と`phase_changed`（引き継ぎやadoptで待てないphaseに組み立て直した、またはadoptで上限を超えた）を足している。`lease_lost`と`run_ended`は書かない（leaseを失ったプロセスは書かない。adoptした側がイベントから同じ待ちを続けるので、書くとその待ちを消してしまう）。代わりに`WaitState::of`は、`run_waiting_started`の後に`lease_acquired` / `lease_released` / `run_recovered` / `runtime_error`があれば待ちは無いとみなす（待ちを持っていたsupervisorがrunを失った。adoptと引き継ぎはこれらを書かない）。

## 引き継ぎとadopt

- **組み立て直し**（`restore_waiting`）: execの引き継ぎ（[Handoff](handoff.md)）とadopt（[`supervise`](supervise.md#adopt)）でslotを組み立てた後、runのイベントから`consumed`（終わった待ちのask）、`deferred`、今の待ち（`WaitState::of`: 最新の`run_waiting_started`の後に`run_waiting_ended`が無ければ待ち、`run_waiting_ended`の後に`run_slot_regained`が無ければ戻り待ち）を戻す。
  待ちに入った時刻はイベントから取り、`run_waiting_started`を記録し直さない。
  組み立て直したphaseが待てないもの（例えばreviewからやり直すrun）なら`phase_changed`で終えてslotに戻す。
  引き継ぎは上限を超えていても待ちのまま戻す（上限を下回るまで新しい待ちを入れない）。
- **adopt**: `adopt_stale_runs`は空きslotの判定を先頭の打ち切りではなくrunごとに行い、イベントの上で待っているrunは待ちの数が上限未満ならslotの空きを要さずに待ちとして引き継ぐ。上限に空きが無ければ今のとおりslotの空きを待って引き継ぎ、待ちを`phase_changed`で終えてslotのrunとして扱う（そのaskでまた待ちに入れる）。

## 空き容量のための掃除

待ちのrunはsupervisorのleaseとslotを持ち、wrapperのheartbeatも続くので、終わったrunと誰も作業していないrunの掃除（[Run worktrees](run-worktrees.md#終わっていないrunのビルド成果物)）の候補にならない。
空き容量のための掃除（[空き容量を確かめる](disk-space.md)の「判定と掃除」）だけが、待ちのrunのビルド成果物も消す。
空きが足りる通常の掃除では消さない（答えはすぐ来ることもあり、作り直しの時間を払わない）。

- 渡し方: queueの一覧はleaseを持つrunを返さないので、leaseを持つsupervisorが、自分のslotの待ちの最中（戻り待ちでない）の非対話のrunを`Supervisor::waiting_worktrees`で掃除に渡す。
- 選び方（loopの上、`cleanup::waiting_candidate`）: runの最新のturnのevent（`turn_requested`・`turn_started`・`turn_finished`）が`turn_finished`で、待ちが持つaskのうち答えも閉じも無いものがあるrunだけを候補にする。
  候補の`WorktreeCleanup::Waiting`はそのaskを持つ。
  待ちはturnの終わりより先に始まりうるので、待ちの始まりではなくturnのeventで見る。
- 消す前（jobの上、予約のlockの中、`cleanup::still_waiting`）: runのstatusが選んだときと同じで、何も動いていないことを確かめ直す。
  sessionのwrapperと最後のturnのagentのpidは、選んだときにrunのevent（最新の`wrapper_started`と`turn_started`）から取る。
  jobのqueueの接続はhost運用のportでleaseと`run_processes`を読まないので、leaseは確かめ直さない: leaseはこの生きているprocessがheartbeatし続けるので、jobのあいだに他のsupervisorが取ることはない。
  動いているとみなすのは、生きている最後のturnのagentと、worktreeで働くかwrapperの下にあるprocess（turnが残したcommandやビルド、cargoも）と、実行ファイルがworktreeの下にあるprocess（`target/`のtestやbuild script）で、processの一覧が読めないときも動いているとみなす。
  待ちのrunはjobの候補の先頭に置き、他のrunの掃除のあいだ答えを待たせない。
  wrapperはturnの間をrun_dirの`runner`から待つだけなので数えない。
- 消している間: jobが通り過ぎるまでrunは予約され、`watch_waiting`は予約されたrunの待ちを何もせずに続ける。
  答えが来ても待ちは終わらず、答えの配送（slotに戻った後の`deliver_answers`）も次のturnも、wrapperを失ったsessionの開き直しも始まらない。
  jobが通り過ぎた後の見張りが答えを見て待ちを終え、いつもの順でslotに戻して答えを届ける。
- 消すもの: 他の終わっていないrunと同じビルド成果物だけで、ソース・未commitの変更・receipt・run_dir・`runner`は残す。
- 消した後: workerの次のturnがsccacheでビルドし直して続く。
  workerのpromptには消したことを書かない: ビルドの成果物はいつ作り直してもよく、cargoが無いものを作り直すので、workerが知らなくても手順は変わらない。
- 触れないもの: turnかcommandが動いているrun、slotに居るrun（着地・validating・review、待ちから戻るのを待つrunも）、対話の経路のrun。

## 登録と見せ方

- 待ちの上限は起動の引数か`dagq.toml`の`[supervisor]`で決まる（[Run environment](run-environment.md)の`[supervisor]`）。
- `status`と`stats`は待ちを出す（[`status`](commands.md)・[`stats`](commands.md)）。
  待ちの数の数え方は`src/domain/waiting.rs`の`WaitCount`の1つで、supervisorと`status`の両方が使う。

## test

wrapper の開き直しと supervisor の配線（slot の外での待ち、引き継ぎと adopt をまたぐ待ち、wrapper の心拍）は integration test が代表の case で確かめる。
待ちの上限・控え・組み立て直しと slot への戻りの判断、および過去を含む待ちの記録と集計は `domain::waiting` と stats の unit test が確かめる。
