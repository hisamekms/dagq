---
id: plan-human-decision-delivery-spike
type: plan
title: スパイク：受け付けた人の判断（ask の答え・send_back の理由・decide）が、届く session の無いまま run の終わり・置き換え・引き継ぎで失われる経路は、task 4018・4019 が塞ぐ 2 つのほかに無いか
status: completed
created: 2026-11-02
owners:
  - hisamekms
tags:
  - planning
  - investigation
related:
  - design-supervisor-lifecycle-ask
  - adr-t2080-1
---

# スパイク：人の判断が届く session の無いまま失われる経路

問いは「受け付けた人の判断（ask の答え・send_back の理由・decide）が、届く session の無いまま run の終わり・置き換え・引き継ぎで失われる経路は、task 4018・4019 が塞ぐ 2 つ（再開を使い切った run への `approve_landing` の `send_back`、`retry`・`retry_inherit` の新しい run に前の run の未適用の判断が渡らないこと）のほかに無い」は成り立つか（goal 252 の受け入れ条件 3）。
方法は読むだけで、build・test の実行・試作・使い捨ての queue はしていない。

## 判定

**does_not_hold（穴がある）。**
4018・4019 の 2 つを前提にしても、重ならない穴が 2 つと、4018 の範囲の書き方しだいで残る穴が 1 つ、頻度の小さい穴が 3 つある。

- **穴 G（頻度が高く、実例がある）**: `[fresh_session]` で同じ run が新しい session に移るとき、前の session に届けた人の判断（`worker_question` の答え・前の `send_back` の理由）は引き継ぎの prompt に入らない。
  直近 30 日の `session_renewed` 82 件のうち 54 件は、同じ run で人の答えを受けた後に起きた。
  goal 252 のきっかけの task 3976 でも起きていた（下の G）。
- **穴 H**: 4018 が渡すのは前の run の「届いていない」判断で、前の run の session に届いた判断は新しい run に渡らない（task 3976 の新しい run は ask 1414・1418・1420 の判断を受けない）。
- **4018 の範囲しだいの穴 D3**: 人の素の `ready`（`--inherit` なし）と hold の解除の `retry_fresh` で作る run。
- **頻度の小さい穴**: replan の置き換え（A）、送ったが取られなかった答えの依頼（D2）、`stalled` の答え（D4）。

## 対象と読んだもの

- **対象の commit**: `77dc759befb35ff2d3dfb7fdac7e7c3fd2076c65`（main。4018・4019 は未着地）。
- **コード**:
  - 答えの配送: `src/application/supervise/execution/session.rs` の `deliver_answers`・`followed_answers`・`follows_ask`、`src/infrastructure/asks.rs` の `undelivered_answers`・`discard_by_runtime`、`src/application/prompt/worker.rs` の `answer_text`
  - 引き継ぎの prompt: `src/application/prompt/worker.rs` の `Inheritance::of`・`Inheritance::section`・`replan_section`・`HandoffMaterial`・`handoff_text`、`src/application/supervise/execution/session.rs` の `ReplanCarried` を組むところと `write_prompt`
  - 置き換え: `src/infrastructure/replan_apply.rs` の適用（置き換えた run の閉じていない ask を閉じるところ）、`src/application/supervise/execution/replan_park.rs` の `owed_and_untaken`
  - resume と開き直し: `src/application/supervise/execution/resume.rs` の `exhaust_resumes`・`write_answers_file`・`carried_answers`、`src/application/supervise/execution/reopen.rs`（module doc・`open_session_again`）、`src/application/supervise/execution/headless.rs` の `prepare_turns`、`src/domain/turn.rs` の `answers_not_started`・`request_seq`・`RequestState`
  - adopt と差し戻し: `src/application/supervise/execution/adopt.rs`（`asks_from: 0`）、`src/application/supervise/execution/revise.rs` の `revise_asks_from`
  - 復旧: `src/application/supervise/execution/triage.rs`（`person_answer`）、`src/domain/recovery.rs` の `live_pass_back`、`src/domain/resume.rs` の `is_inherit_retry`・`retried_with_inheritance`、`src/domain/worker_question.rs` の `closed_undelivered`
  - edit: `src/domain/task.rs` の `content_editable`、`src/infrastructure/sqlite.rs` の `edit_task_authorized`（`in_progress` の task は run が終わった後の verify だけ）
  - planner への答え: `src/application/prompt/planners.rs`（「The planner before you asked a person ... and is gone」の節）
- **ADR**: ADR-0022（ask と answer）、ADR-0047 決定 24・39（再開の使い切り）、ADR-0071（slot の外の待ち）、ADR-t1521-2（置き換え）、ADR-t1879-1（hold）、ADR-t1962-1（`ready --inherit`）、ADR-t2080-1 決定 2（新しい session の引き継ぎの prompt）、ADR-t2946-1（人の send_back の再開）。
- **design**: `docs/design/supervisor-lifecycle/` の ask・waiting・needs-session（「再計画で保留したrun」「taskのhold」）・handoff・queue-hold・triage・headless-worker（「待ちの最中に失ったsessionの開き直し」）・review の 8。
- **本番の events**: 2026-10-03〜2026-11-02（直近 30 日）。kind ごとの件数:
  - 全体: `ask_answered` 1,081、`ask_delivered` 155（run に紐づくもの 50）、`landing_decided` 344、`session_renewed` 82、`task_released` 17、`hold_answer_applied` 19、`replan_applied` 1、`replan_carried` 0、`ask_delivery_failed` 0
  - run ごと: `ask_delivered` のある 39 run の `turn_requested`・`turn_started`
  - 実例: 3 run（81be9ecb・5acd51f8・493e949a）の ask と `session_renewed`・`landing_decided`

## 経路ごとの判定

4018・4019 が塞ぐ 2 つは塞がれたものとして判定する。

### A. replan の置き換え：失われる（頻度は小さい）

- 適用は、置き換えた run の閉じていない ask を、答えがあってもなくても `discard_by_runtime`（reason `replaced`）で閉じる（`src/infrastructure/replan_apply.rs` の「The run's asks can no longer be applied to it」）。
  答えを子の task に移すものは無い。
- 子の run の prompt の `replan_section` は成果（commit か未 commit の差分）だけを渡し、元の run の判断を持たない。
  `ReplanCarried` にも判断の欄は無い。
- 保留（`replan_parked`）の間に来た答えは `owed_and_untaken` が `carried_answers` に残すが、それを読むのは継続の resume だけで、適用（置き換え）では読まれない。
- 救われる部分: 置き換えの planner は `dagq show` で元の task の ask を読める（`planners.rs` の置き換えの prompt）。
  ただし prompt は判断を子へ写せとは言わず、`show` の既定は文を切る。
- 実例: 30 日で `replan_applied` は 1 件（event 1114043、task 1669）。
  閉じた ask 1369 は未回答の `decide`（event 1114042）で、人の判断は失われていない。

### B. resume の上限（`approve_landing` 以外の ask）：届く（4018 を前提に）

- 再開を使い切った run は `exhaust_resumes` で `failed` になり、復旧 job に回る。そこでは resume が選択肢に無い（`resume.rs` の `exhaust_resumes` の doc comment、ADR-0047 決定 39）。
  次は `retry`・`retry_inherit` なので、届いていない `worker_question`・`decide` の答えは 4018 の範囲。
- `decide` の答えは runtime が適用する（`retry`・`resume`・`cancel`）。job の選択肢を選んだ答えは、次の job の依頼の `person_answer` で job に届く（`triage.rs`）。
- 生きている run の `stalled` の答えは `live_pass_back` で次の job に届く。
- 30 日の人の `worker_question` の答え 50 件は、どれも `ask_delivered` があり、その答えの依頼の番号そのものを `request` に持つ `turn_started` がある（下の D2 の照合）。

### C. supervisor の引き継ぎと adopt：届く

- 答えは queue の行にあり、adopt した `SessionWatch` も最初の session と同じく `asks_from: 0` で始まる（`adopt.rs`）。そのため、閉じていない答えは、session が turn の間に入った後の `deliver_answers` が送る。
- 引き継ぎの drain の間は配送を進めないが、遅れるだけで捨てない（`handoff.md` の drain の節）。
- 差し戻しと解消依頼の依頼は `adopted_delivery`・`requested_seq` が、書いたかどうかを `turns/` から判定する。そのため、二重にも欠けにもならない（`src/domain/turn.rs`）。
- `queue_hold` の答えは `hold_answer_applied` の印を先に書き、引き継ぎの後は slot を組み立て直してから適用する（`queue-hold.md`）。
  これは認証と利用上限の答えで、task の中身の判断ではない。

### D. run の中断・時間切れ・hold・cancel

- **D1. hold の解除で同じ run を resume する（`resume`、30 日で 13 件）：届く。**
  保留の時に turn が始まっていなかった答えは `owed_and_untaken` が `carried_answers` に残し、解除の後の resume の依頼に載る（`replan_park.rs`、`resume.rs` の `carried_answers`、`needs-session.md`「taskのhold」）。
  hold の解除で run を終えて成果を引き継ぐときは、手での `retry_inherit` の `triage_finished` を書く（`is_inherit_retry`、`src/infrastructure/task_holds.rs`）ので 4018 の範囲。
  引き継ぐ成果の無い解除（`retry_fresh`）はその記録を書かず、下の D3 に当たる。
- **D2. 送ったが session が取らなかった答えの依頼：失われる（30 日で 0 件）。**
  `deliver_answers` は依頼を `turns/` に書いた時点で `ask_delivered` を記録し、ask を閉じる。
  wrapper がそれを取る前に session が失われ、resume か開き直しで新しい wrapper を起動すると、`prepare_turns` は取られなかった依頼を `.dropped` にする（`headless.rs`）。
  ask はもう閉じているので、配送は 2 度目を送らない。
  resume の依頼に答えを載せる `carried_answers` は、柵による保留（replan・hold）の後の resume でしか作られない（`resume.rs`）。
  照合（2026-10-03〜2026-11-02T14:41Z）: run に紐づく `ask_delivered` 50 件（39 run）を見た。それぞれの ask について、`what` が `answer of ask <id>` の `turn_requested` から答えの依頼の `seq` を取った。その `seq` と `request` が一致する `turn_started` があるかを照らした。
  50 件すべてに、答えの依頼そのものから始まった `turn_started` があった（`turn_requested` の無いものも、一致する `turn_started` の無いものも 0 件）。
  `.dropped` にした依頼から turn は始まらず、依頼の番号は run の `turns/` の中で `.dropped` を含めて重ならない（`next_seq`）。そのため、この 50 件で答えの依頼が落ちたものは無い。
  確かめていない範囲: 30 日より前と、turn が始まった後に provider が答えを会話に残さずに失敗した turn。
- **D3. 人の素の `ready` と hold の解除の `retry_fresh`：4018 の範囲の書き方しだいで失われる（`retry_fresh` は 30 日で 1 件）。**
  どちらも前の run を終えて、引き継ぎの無い新しい run を始める（`triage.md` の 105 行の「人が`ready`を打った」、`needs-session.md`「taskのhold」の `RetryFresh`）。
  届いていない答えは「終えたrunに残り、新しいrunの依頼には載らない」と design が書く（`waiting.md`・`needs-session.md`）。
  goal 252 の受け入れ条件 2 が挙げるのは runtime・復旧の job・人の `ready --inherit` だけ。
  4018 が、前の run を `retried_with_inheritance` や `retry` の記録から探すなら、この 2 つは漏れる。
- **D4. `stalled` の ask の答え（指示の文）：失われる（30 日で人の答え 0 件）。**
  答えは slot に戻った `StallWatch` が送る（`waiting.md` の 5）。
  送る前に run が失敗して retry になると、4018 が名指すのは `worker_question`・`decide` だけなので、指示の文は渡らない。
- **D5. 時間切れ・失った wrapper（答えを待つ間）：届く。**
  `reopen.rs` が同じ session を開き直し、失った turn の idle marker を書いて、答えを次の turn で受けられるようにする。
  上限（`REOPEN_ATTEMPTS`）を越えると、終わった session と同じく復旧 job に回る。そこで届いていない答えは 4018 の範囲。
- **D6. cancel：届く必要が無い。**
  task が終わり、判断を適用する作業が残らない。runtime は残った `approve_landing` を閉じる（`ask.md`）。

### E. edit・revisit の後の run：届く

- `in_progress` の task の edit は、最新の run が `failed`・`interrupted` で生きた run が無いときの `--verify` だけ（`sqlite.rs` の `edit_task_authorized`）。
  その後は復旧 job の `edit the task's --verify, then retry_inherit`（`recovery.rs` の `VERIFY_FIX_OPTION`）で `retry_inherit` に進むので、4018 の範囲。
- 中身の edit は draft・submitted だけ（`task.rs` の `content_editable`）で、run がまだ無い。
- revisit は draft の planner を開き直す。前の planner の `planner_question` の答えは、新しい planner の prompt に本文で入る（`planners.rs` の「The planner before you asked a person (ask …) about draft … and is gone」）。
  request・finding・置き換えの planner も同じ形で前の planner の答えを受け、plan review の待ちの planner は答えを新しい planner に渡す（`plan_review.rs`）。

### F. `ready --inherit` と `ready`

- `ready --inherit`：4018 の範囲（goal 252 の受け入れ条件 2）。
- 素の `ready`：上の D3。

### G. `[fresh_session]` の新しい session（同じ run）：失われる（30 日で 54 件の機会）

この経路は task の挙げた一覧に無く、調べる中で見つけた。

- 差し戻し・resume で直前の turn の `peak_context` が閾値を超えると、同じ run のまま会話を引き継がない新しい session を、引き継ぎの prompt で始める（ADR-t2080-1 決定 1・2）。
- 引き継ぎの prompt（`worker.rs` の `handoff_text`・`HandoffMaterial`）が持つのは次のものだけ:
  - commit・未 commit の変更・手元の検証
  - 既知の失敗
  - receipt の要約
  - 送るはずだった依頼（最新の差し戻しの理由）
- 前の session に届けた `worker_question` の答えと、前の回の `send_back` の理由は持たない。
  文は「nothing of the earlier conversation carries over」と書く。
- 実例:
  - **run 81be9ecb（task 3976）**:
    - 人の判断は 4 件: ask 1414 の send_back（event 1159350）、ask 1418 の send_back（event 1160006）、ask 1418 の理由は ask 1414 の判断を名指す、`worker_question` 1420 の答え（`ask_delivered` event 1160537）
    - 10:05Z に `session_renewed`（event 1161201、`peak_context` 437,032）で新しい session に移った
    - その後の ask 1422 の send_back の理由（event 1161334）は「ask 1420 の A（…）は人の判断で、受け入れ 3 の変更として扱ってよい」と、人が前の判断を言い直している
    - goal 252 が記す古い方針のままの作業（ask 1424 以降）の手前で、すでに判断が会話から落ちていた
  - **run 5acd51f8（task 3745）**: `worker_question` 1360・1362 を届けた（event 1097192・1099451）後に `session_renewed`（event 1102121）。
  - **run 493e949a（task 3107）**: `worker_question` 1170 を届けた（event 318008）16 分後に `session_renewed`（event 318169）。
- 件数: 30 日の `session_renewed` 82 件のうち 54 件で、その前に同じ run の人の判断があった（`ask_delivered`、または `approve_landing` の `send_back` の `landing_decided`）。
  そのうち 20 件は判断が 2 件以上で、引き継ぎの依頼に載りうる最新の 1 件のほかの判断が落ちる。
  6 件は前の session に届けた `worker_question` の答えを含む。
  残りの判断が 1 件だけのものも、新しい session に移ったのがその send_back の resume でなく後の review の差し戻しなら、その理由は落ちる。

### H. retry の新しい run に、前の run で届いた判断：失われる（4018 の範囲の外）

- 4018 が渡すのは「最後の send_back の理由、`worker_question`・`decide` への答えのうち session に届いていないもの」。
- 前の run の session に届いた判断は、commit に表れていなければ新しい run に渡らない。
  渡るのは `Inheritance::section` の receipt の要約か、受け継いだ commit だけ。
  例: task 3976 の新しい run 88eee852 には、4018 の後でも ask 1414・1418・1420・1422 の判断は入らない。
- これは G と同じ形（同じ task の作業を続ける次の会話に、前の会話が受けた人の判断が無い）。

## 塞ぐ task の案と見送り

### 案 1（G と H。推奨: goal 252 に実装の task を足す）

- **何を**: run（`retry`・`retry_inherit` では task の前の run も）が受けた人の判断を、新しい会話の最初の prompt に本文で入れる。
  判断は、`approve_landing` の `send_back` の理由、`worker_question` の答え、復旧 job の `decide` の答えのうち session に送ったもの。
  4018 の「届いていない判断」の節と同じ置き場にまとめ、届いたか否かを添える。
  上限で切るときは ask の id と `dagq events --run` を名指す（goal 252 の制約）。
- **変える関数**:
  - `src/application/prompt/worker.rs` の `HandoffMaterial`（判断の欄を足す）と `handoff_text`
  - 4018 が `Inheritance` に足す節
  - 判断を events から集める domain の関数（`RunHistory` の近く）
  - `src/application/supervise/execution/` で `HandoffMaterial` を組むところ
- **test の層**:
  - 判断の集め方は domain の unit test
  - 節の文と上限は `src/application/prompt/tests.rs`
  - 配線は、send_back と `worker_question` の答えの後に `[fresh_session]` で新しい session に移った run の引き継ぎの prompt に両方の本文が入ることを、supervise の配線の層の test で確かめる
- **触る文書**: ADR-t2080-1 決定 2 の引き継ぎの prompt の中身を足す（amends の ADR か design）。
  あわせて `docs/design/provider-lifecycle.md` の新しい session の節と、`docs/design/supervisor-lifecycle/review.md`。

### 案 2（D3。推奨: 4018 の受け入れで確かめるか、小さな task を足す）

- **何を**: 前の run を、retry の記録からではなく task の直前の run として探す。そうすれば、人の素の `ready` と hold の解除の `retry_fresh` の新しい run も、未適用の判断を受ける。
- **変える関数**: 4018 が足す、前の run の未適用の判断を集める関数と、その呼び出し（`session.rs` の `write_prompt`）。
- **test の層**: 配線の層。`retry_fresh` の解除の後と、素の `ready` の後の新しい run の prompt に send_back の理由が入ることを確かめる。
- **触る文書**: `docs/design/supervisor-lifecycle/waiting.md`・`needs-session.md` の「届いていない答えは…新しいrunの依頼には載らない」の文。

### 案 3（A。推奨: 頻度が小さいので、案 1 の関数を使う小さな task にするか見送る）

- **何を**: 置き換えの適用が閉じる ask のうち答えのあるものを、子の run の `replan_section` と置き換えの planner の prompt に本文で入れる。
  `replan_applied` の `closed_asks` で答えのあったものを区別する。
- **変える関数**:
  - `src/infrastructure/replan_apply.rs` の適用（答えのある ask の記録）
  - `ReplanCarried`（判断の欄）と `replan_section`
  - `planners.rs` の置き換えの planner の prompt
- **test の層**: 適用は infrastructure の test、prompt は `prompt/tests.rs`。
- **見送りの理由になりうるもの**: 30 日で置き換えは 1 件、答えのある ask を閉じた例は 0 件。
  置き換えの planner は `dagq show` で ask を読め、置き換えは人の見る planner の proposal を通る。

### 見送りの推奨（D2・D4）

- **D2（送ったが取られなかった答えの依頼）**:
  - 見送りの理由:
    - 30 日の 50 件で、答えの依頼の番号そのものから始まった turn が全部にあり、落ちた例は無い
    - 起きるのは、依頼を書いてから wrapper が取るまでの短い間に wrapper が失われたときだけ
    - 起きれば、人の答えは ask の行と events に残り、ask は閉じて見える
  - 塞ぐなら: `prepare_turns` で `.dropped` にする依頼のうち `turn::answered_ask` が答えと言うものを、新しい session の依頼に載せる（replan の保留の `owed_and_untaken` と同じ形）。
    test は `domain::turn` の unit と、resume の配線の層。
- **D4（`stalled` の答え）**:
  - 見送りの理由:
    - 30 日で人が答えた `stalled` は 0 件
    - `stalled` の答えは `wait`・`stop` と指示の文で、run が失敗した後は復旧 job が別の判断をする
  - 案 1 か 4018 の節が `stalled` の指示の文も集めれば、追加の費用は小さい。

## unresolved の経路

コードと 30 日の events で判定できなかった経路は無い。
ただし D3 は、対象の commit では失われ、未着地の 4018 がそれを塞ぐかは 4018 の実装を読まないと分からない。
判定に要るのは、4018 の着地した commit で、前の run の未適用の判断を集める関数が、前の run をどう探すかを読むこと。
retry の記録から探すなら穴として残り、案 2 の task が要る。
4018 と goal 252 の範囲は、task と goal の文に依っていて、コードでは確かめていない。
