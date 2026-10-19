---
id: design-supervisor-lifecycle-handoff
type: design
title: "Handoff"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-up-down
  - adr-0045
  - design-persistence
  - adr-0062
  - design-supervisor-lifecycle-waiting
---

# Handoff

[ADR-0045](../../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定10・11。
supervisorは、sessionの終わりを待たずに、自分のpidのまま別のbinaryをexecして続ける。
use caseは`src/application/supervise/host_ops/handoff.rs`（登録と要求）と`execution/handoff_slots.rs`（slotの止めと取り戻し）、execは`src/main.rs`の`run`。

- **要求**: 引き継ぎを受けられる印（`supervisors`の`handoff_accepted`。`supervise`が登録の直後に付ける）のある登録にだけ、`up`か`install`が`request_handoff(token, binary)`で要求を書く。
  受けられない登録（引き継ぎを知らない前のbinary）には書かず（`false`）、`up`はdrainに、`install`は`not_handed_off`に回す。
  要求をDBに置くのは、signalはlaunchd modeと登録済みのin-cmuxのsupervisorで届け方が違い、区切りまで待たせる必要もあるため。
- **区切り**: ループの各passの先頭で、停止要求（SIGINT/SIGTERM）が無ければ`handoff_request(token)`を読む。
  要求があれば新しいclaim・adopt・resume・triage・observer・plan review・goal reviewの起動を止め、DBとrunのファイルから組み立て直せないslot（検証と着地が進行中のもの。`Phase::rebuildable`）だけを進める（`tick(true)`）。
  待つあいだも各passで、走っているplan review / goal reviewのjobが終わっていれば回収してverdictを適用する（verdictでreadyになったtaskは待ちのあいだclaimせず、次のプロセスがclaimする）。
  全slotが組み立て直せる状態になったら、`prepare_handoff`はobserverを止めた後、plan review / goal reviewのjobを1回pollし、終わっていれば回収してverdictを適用する（終わったjobのverdictを捨てて次のプロセスで同じattemptをやり直さない）。
  そのうえでobserverの子プロセスと、まだ走っているheadlessのreview / triage / plan review / goal reviewのjobを（その子孫ごと）止める（`headless_jobs`の行は`stopped`で閉じ、execの前に書く。[Headless job processes](headless-job-processes.md)）。
  plan review / goal reviewの行は同じtokenのまま未完了で残り、引き継いだプロセスが最初のpassに入る前に`interrupted`で閉じ、候補のままなら以後のpassで再びreviewする（[Plan review](plan-review.md)・[Goal review](goal-review.md)）。
  triageはleaseを返し、次のプロセスがtriageし直す。
  reviewはleaseを保ち、次のプロセスがreviewし直す。
  DBが持たない状態（resume中のsessionと、検証で`failed` / `needs_session`になって終了を待つrunのsession。欄は`handoff::Snapshot`）を、書いたsupervisorのtokenを添えて`runs/<id>/handoff.json`に書く（同じtokenの次のプロセスだけが読み、それ以外が残したfileは読まずに消す）。
  そのあと結果`handoff`でループを抜ける。
  登録は消さない（heartbeatだけが止まり、execは数秒で終わるのでlease TTLを越えない）。
  停止要求は引き継ぎに勝ち、通常のdrainになる。
- **要求の受け取りと取り消し**: drain中のsupervisorは各passで`handoff_request(token)`を読み直し、取り消されていれば（None）保持した要求を捨ててdrainを解き、同じpassで通常のclaimに戻る。
  別のbinaryの要求に置き換わっていれば（`request_handoff`は`handoff_binary`を上書きするので、`install`と自動更新が重なると起きうる）、claimを挟まずに新しいbinaryを保持してdrainを続け、そのbinaryでexecする（logは「置き換わった」と「取り消された」を分けて出す）。
  execの直前には`take_handoff(token, binary)`の条件付きUPDATEで要求の列を原子的に消し、`handoff_accepted`を0にする（取り戻しの`resume_registration`が1に戻すまで次の要求を受けない）。
  `hand_off`はこの「要求が無く`handoff_accepted`が0の同じtokenの行」をexecの途中と見て待ち、プロセスが死ねば失敗、新しいbuild識別子で戻れば成功、前のbuild識別子で戻ればexecの失敗とする。
  tokenとbinaryが一致して受け取れたときだけ`prepare_handoff`を行って`handoff`を返す。
  受け取れなければexecせず、`handoff_request`を読み直して、別のbinaryの要求があればそれを保持してdrainを続け（次のpassでそのbinaryの受け取りを試みる）、無ければdrainを解いて通常のclaimに戻る。
  取り消しでdrainを解くとき（各passの読み直しとexec直前の受け取りのどちらでも）は、終わったrunの掃除も通常に戻す（`resume_cleanup`）: 頼みを再び受け、共有の停止の印（`cleanup.stop`）を下ろし、終わったrun全部の掃除をすぐ頼み直して、drainのあいだ捨てた頼みと、停止の印で今のworktreeの後に止まったjobの残りを拾う。
  空き容量の掃除はdrainで受けられなかった頼みを掃除の間隔（`CLEANUP_INTERVAL`）に数えないので、戻った後の容量不足はすぐ掃除のjobを走らせ、それでも足りないときだけdiskのaskになる。
  置き換えと停止要求でのdrainでは掃除を戻さない。
  受け取り後の`cancel_handoff`はfalseを返すので、呼び出し側は`HANDOFF_GRACE`のあいだ新しいbinaryで戻ったかを見直す。
- **停止要求の記録と引き継ぎの待ち**: supervisorは停止要求（SIGINT/SIGTERM。どこから来ても）に最初に気づいたpassで、queueのevent `supervisor_draining`をプロセスごとに1回だけ記録する（書けなければlogに残して次のpassで書き直す）。
  停止要求は引き継ぎに勝つので、drainを続けてexecしない。
  `hand_off`の待ちは各`poll`で最新の`supervisor_draining`を読み、待っているsupervisorがまだ引き継いでいないのにそのtokenの`supervisor_draining`があれば、上限を待たずにそのsupervisorを失敗にし、要求を`cancel_handoff`で取り消す（要求の前の記録も同じく、その停止はexecせずに終わる。drainを終えて登録が消えた後も同じ）。
  1回の見直しは停止の記録・登録・停止の記録の順に読む（`Stops::around`）: まだ要求を持つsupervisorは登録より前に読んだ停止だけで停止中とし（新しいbuildで戻った直後に止められたsupervisorを停止中と取り違えない）、登録が消えた・heartbeatが古くなったsupervisorは登録の後に読んだ停止でも停止中とする（supervisorは登録を消す前に停止を記録するので、2つの読み取りのあいだに停止して登録を消したsupervisorも停止中と分かる）。
  この失敗は`Handed`の`stopping`がtrueになり、`Handed::report`の要素に`stopping: true`が付く。
  停止中による失敗も、全員の失敗で前のbinaryに戻し一部なら新しいbinaryを残す規則（ADR-t632-1）と同じaskの形に従う。
  上限切れのerrorに「検証や着地がまだ進行中」と添えるのは、停止中でないsupervisorだけにする。
- **exec**: `main`は`supervise`の結果が`handoff`なら、queueの接続がすべて閉じた後で、自分の引数を引き継ぎの引数に組み直し（`handoff_arguments`、ADR-t813-2）、要求されたbinaryを`CommandExt::exec`で同じpidのまま起動する（SQLiteのfdはclose-on-execで開かれる）。
  execが失敗したら（fileが無い、実行できない）logに残し、同じbinaryのまま同じ引数で`supervise`をやり直すので、登録は前のbuild識別子で取り戻され、頼んだ側は「別のbuild識別子で戻った」として失敗を知る。
  登録済みのin-cmuxのsupervisorも同じプロセスのままで、launchd modeのsupervisorは同じpidなので`KeepAlive`は何もしない。
- **取り戻し**: exec 後は同じ token と pid の登録を取り戻し、version と heartbeat を更新する。
  lease と開始時刻を保ち、supervisor_handed_off を記録して slot を復元する。
  handoff.json の resume と終了要求、送った依頼を復元し、依頼を二重に送らない。
  snapshot が無ければ生きている wrapper の run を adopt と同じ条件で復元する。
  復元した resume は resume_adopted を記録する。
  review 中（`awaiting_integration`）の run は adopt と同じ `adopt_review` で組み立て直すので、記録した revise・衝突の依頼が `turns/` に無ければ、その試行の依頼だけを照合して 1 回だけ書く（[Review](review.md) の 7）。
  既に他の supervisor が取った lease には触れない。
- **resumeの依頼を送る前**: resumeを始めて（`resume_started`・`session_opened`）から`resume_request_sent`までの待ち（wrapperとagentの登録）は、queueのprocessの行をpassごとに読むだけで、passを塞がない（`ResumeWatch`）。
  依頼の送信（`submit`）はsessionの次のturnの依頼を書くだけで、画面を読まない。
  resumeのslotは組み立て直せるので、この間に頼まれた引き継ぎは次のpassの先頭で受け取られ、`handoff.json`は`message_sent_at`なしで書かれ、次のプロセスは同じsessionに依頼を1回だけ送る（runをresumeし直さない。登録とresume timeoutの時計は引き継いだときから数え直す）。
- **待ち**: 人の答えを待つrun（[人の答えを待つrun](waiting.md)）のslotは組み立て直せるので、引き継ぎはそれを待たない。次のプロセスはslotを組み立て直した後、runのイベントから待ちと戻り待ちを戻す（`run_waiting_started`を記録し直さず、上限を超えていても待ちのまま。[ADR-0062](../../adr/0062-runs-waiting-for-a-person-leave-the-slot.md)の決定11）。
- 走っているrunのwrapperはclaim時の`runner`（前のbinaryのコピー）で動き続ける。互換の範囲のschemaならそのまま動く。
- **run の review job と復旧 job**: `tick(true)`は組み立て直せるslotを進めないので、run の review job（`Review`）と復旧job（`Recovery`）は、引き継ぎを待つあいだに終わってもverdictを回収されず、`prepare_handoff`で止められて次のプロセスでやり直す（reviewはleaseを保ってreviewし直し、復旧jobはleaseを返して取り直す）。
  plan review / goal reviewと違って回収しないのは、verdictの適用がslotを着地や`/exit`へ進めて引き継ぎの待ちを延ばしうるため。
