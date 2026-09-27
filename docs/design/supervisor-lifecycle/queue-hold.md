---
id: design-supervisor-lifecycle-queue-hold
type: design
title: "認証と利用上限のaskの待ちとanswer"
status: current
created: 2026-09-27
updated: 2026-09-27
last_verified: 2026-09-27
scope: runtime
related:
  - adr-0047
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-ask
  - design-supervisor-lifecycle-claim-hold
  - design-supervisor-lifecycle-disk-space
  - design-supervisor-lifecycle-waiting
  - design-supervisor-lifecycle-status
---

# 認証と利用上限のaskの待ちとanswer

Claude Codeのログインが切れるか利用上限に達すると、新しいrunもheadlessのjobも同じ理由で止まる。runtimeはそれをqueueで1件の`queue_hold`のask（`authentication`、または`cost`で`subject: usage_limit`）にまとめ（task 361、[ask](ask.md)）、openな間は新しいclaimとheadless jobの起動を控え、人のanswerを自分で適用する（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定42の「待ち」と「answer」、task 437）。ディスクの`cost`のask（`subject: disk`、options `done` / `wait`）は[空き容量を確かめる](disk-space.md)（task 377）が扱い、ここの控えとanswerの適用には入らない。

## どのaskが控えるか

`domain::queue_hold::reason_of`: kindが`queue_hold`で、理由が`authentication`なら`HoldReason::Authentication`、`cost`で`subject`が`usage_limit`（`USAGE_LIMIT_SUBJECT`）なら`HoldReason::UsageLimit`。`subject`が`disk`のものは対象外で、それ以外の`subject`の`cost`は利用上限と同じに扱う。`hold_of`はそのうちopen（未回答）のものだけを控えにする。

## 控え

supervisorは毎pass（drainの途中も）、ディスクの確認（`check_disk`）の次に`Supervisor::check_queue_hold`でqueueのcloseされていない対象のaskを読み、回答済みのものを適用してから、openなものの最初の1件を`queue_hold`（`claim_hold::QueueHold`: 理由、`ask_id`、`affected`の数）として持つ。持っている間:

- **claim**: `ClaimHold::judge`は`HoldInputs.queue_hold`をディスクとloadより先に判定し、`claim_held`（`reason`が`authentication` / `usage_limit`、`value`がaskの`affected`の数、`threshold`が0、`ask_id`、`message`）を記録してclaimしない。askが閉じるか回答されると次のpassで`claim_resumed`になる（記録の規則と`status`の`claim_hold`・`stats`の`claim_holds.by_reason`は[claimを控える](claim-hold.md)と同じ）
- **review**: 受理されたrunのreviewは`Phase::ReviewHeld`で待ち、sessionは開いたまま、`review_started`を書かない（`start_review` / `retry_review`が入口で判定する）。控えが解けたpassで`start_review`が始める。引き継ぎとadoptは`awaiting_integration`のreviewの無いrunを`start_review`で組み立て直すので、同じく待つ
- **復旧job**: 終わったrunのtriage（`triage_runs`）を始めない。生きているsessionのalertの復旧job（`RecoveryWatch::start`）も始めず、alertは控えが解けた後のpassでまた拾う（長く走るbackgroundのalertは、`seen`を進める前に判定するので失われない）
- **plan review・goal review・observer**: 起動しない（`plan_review_pass` / `goal_review_pass`に`starting: false`、`start_observer_when_due`を呼ばない）。走っているjobは最後まで追う
- 走っているrunはleaseとsessionを持ったまま進む。着地（`integrate`）はClaudeを使わないので控えない

`queue_hold`のaskに入った（`affected`に居る）runの促しとstalledのaskを止める扱い（`hold_of`）と、待ちから`queue_hold`で戻す扱い（[人の答えを待つrun](waiting.md)）はtask 361のまま。促しを止めるのはaskが閉じるまで（`hold_unclosed`。answerの後、supervisorが`done`を適用して続けてよいという文を打つまでの間も含む。task 729）で、その間に促しが先に打たれることはない。

## answerの適用

answerがaskのoptionsのどれか（`queue_hold::applies`）なら、`answer`は`ask_answered`に`runtime_delivers: true`を書き、`status`はattentionを`applying the answer of ask N (runtime)`（`ApplyingAnswer`）として人の操作を求めない。ディスクのaskの`done` / `wait`も同じ（supervisorが`apply_disk_answers`で適用する）。optionsに無い自由な答えは`runtime_delivers: false`で、inboxが人に見せる（`read the answer of ask N and close it`）。回答済みのaskは控えない。

- **`done`**（`Supervisor::hold_done`）: `affected`のrunのうち、このsupervisorのslotで生きているsessionを見ている段（`Phase::Session` / `Revise` / `Resume`、`Phase::holds_live_session`）に居るものを`hold_continue`に入れ、そのsessionの監視（`SessionWatch::continue_after_hold`。workerのsessionではwrapperが生きていて`/exit`前、receiptが無いとき、reviseとresumeでは答えの配送と同じ位置で、送った後はその段の時間切れを数え直す）が決まった文面`queue_hold::CONTINUE_TEXT`を1回送る。送信は`submit`で確かめ（入力欄に残ればEnterを送り直す）、`StartCheck`で取り込まれたかを見る（ADR-0047の決定31）。送ったらrunに`hold_continue_sent`（`ask_id`、`workspace_id`、`submitted`）を書き、stallの計時は送った時刻からやり直す。送れなかったらlogだけ残し、stallの促しが続きを頼む。receiptの後やreview・exitの段に進んだrunには送らない。別のsupervisorのslotのrunは`elsewhere`に書くだけで、そのsupervisorのstallの促しが続きを頼む。続けて、askが開いた時刻の600秒前（`FAILED_BEFORE_ASK_SECS`。askはsessionかjobがエラーを見せてから開くので、その直前に記録された失敗を含める）以後に失敗したjobを起動し直す:
  - 終わったrunの復旧job（`triage_failed`）: runが`failed` / `interrupted`でleaseが無く、triageの状態が`Failed`でその失敗が最新なら、runに`job_restarted`（`job: triage`、`ask_id`、失敗の`event_id`）を書く。`triage_state`は`job_restarted`（`job: triage`）を`Pending`に戻すので、次のpassの`triage_runs`が拾う（試行回数は数え続け、上限を超えれば従来どおりaskになる）
  - plan review（`plan_review_failed`）: proposalがまだ失敗で止まっていれば、`submit --proposal ID`と同じく出し直す（`proposal_resubmitted`）
  - goal review（`goal_review_failed`）: goalがまだ失敗で止まっていれば、`goal review ID`と同じくrearmする（`goal_review_rearmed`）
  - 起動し直さないもの: runのreviewの失敗は失敗のときに`approve_landing`のaskを開いている（task 328）ので人の答えに任せる。生きているsessionの復旧jobの失敗（`recover by hand`）はsessionが進めば消え、次のalertで新しいjobが始まる。observerは次の間隔で動く

  最後にaskを閉じ（`ask_closed`）、queueに`queue_hold_applied`（`ask_id`、`answer`、`reason_category`、`subject`、`continued`、`released`（空）、`moved_on`（このsupervisorのslotに居るが、もうsessionの段に居ないrun）、`restarted`（`{job, run_id | proposal_id | goal_id}`の配列）、`elsewhere`、`supervisor`）を書く。
- **`cancel_affected`**（`Supervisor::hold_canceled`）: `affected`のrunのうちこのsupervisorのslotで生きているsessionの段（`Session` / `Revise` / `Resume`）かreviewの待ち（`ReviewHeld`）に居るものを、slotから外してheadless jobを止め、`abandon`と同じく手放す: `runtime_error`（`message`、コード`hold_canceled`、`lease_released: true`）を書いてleaseを返す。statusは変えず、sessionとworktreeは残る。inboxには`recover run`のattentionとして出る（成果を捨てるかどうかは、この答えを選んだ人の判断で、`recover`の手順で決める）。このsupervisorのslotに居ても自分で先へ進んだrun（validation・review・着地の途中）はもう止まっていないので手放さず、`moved_on`に書く。別のsupervisorのslotのrunは`elsewhere`に書く。askを閉じ、`queue_hold_applied`（`released`に手放したrun、`moved_on`、`elsewhere`。どのrunもこの3つのどれか1つにだけ載る）を書く。

同じqueueに複数のsupervisorが居るときは、最初に回答を読んだsupervisorが自分のslotのrunに適用してaskを閉じる。`elsewhere`のrunは、`done`ならそのsupervisorのstallの促しが続きを頼むが、`cancel_affected`は適用されない（inboxが`queue_hold_applied`の`elsewhere`を見て`recover`で手放す）。引き継ぎ（exec）の後のsupervisorは、runを組み立て直す前に控えを読むので、待っていたreviewを始めない。

## 記録とtest

- queueイベント: `claim_held` / `claim_resumed`（理由`authentication` / `usage_limit`、`ask_id`）、`queue_hold_applied`（`QUEUE_EVENT_KINDS`）
- runイベント: `hold_continue_sent`、`job_restarted`、`runtime_error`（`hold_canceled`）
- test: `tests/it/runtime_queue_hold.rs`（利用上限のaskの控えでclaimとreviewが待ち、`done`で再開する／`cancel_affected`でrunを手放す／`done`で失敗したtriageを起動し直す）、`tests/it/runtime_stall.rs`の`an_idle_session_at_a_login_that_ran_out_waits_in_the_authentication_ask`（ログインの切れで控え、`done`で続きの文面を送る）、`domain::queue_hold`・`domain::claim_hold`のunit test

利用上限の検知、headless jobの出力からの検知、jobをaskの`affected`に足すことはtask 438が扱う。
