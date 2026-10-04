---
id: design-supervisor-lifecycle-claim-defer
type: design
title: "claimを控える（衝突の多いファイル）"
status: current
created: 2026-09-26
updated: 2026-10-05 # task 1634
last_verified: 2026-10-05 # task 1634
scope: runtime
related:
  - adr-0080
  - adr-t1484-1
  - adr-t1634-1
  - adr-t813-2
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-claim-hold
  - design-supervisor-lifecycle-conflict-thresholds
  - design-supervisor-lifecycle-status
  - design-supervisor-lifecycle-stats
---

# claimを控える（衝突の多いファイル）

候補のtaskが触ると予想するファイルと、進行中のrunのファイルが、衝突の多いファイル（hotspot）で重なるとき、supervisorはそのpassでそのtaskをclaimせずに次の候補をclaimする（[ADR-0080](../../adr/0080-supervisor-rereads-conflicts-config.md)、ADR-0069を統合。goal 45、task 463・585）。queue全体のclaimを止める[load averageの控え](claim-hold.md)とは別で、1つのtaskだけを飛ばす。判定は`domain::claim_defer`、supervisorの側は`application/supervise/claim_defer.rs`（`Supervisor::claimable`）。

## 判定の入力

- **hotspot**: `stats`の`conflict_hotspots`（既定のwindow）で`alert`のファイル（mainから消えたものを除き、名前が変わったものは今の名前）。閾値は`dagq.toml`の`[conflicts]`（[Conflict thresholds](conflict-thresholds.md)）。plan reviewのpromptと同じ計算（`Supervisor::conflict_hotspot_files`）で、10分ごとと、`[conflicts]`の値が変わったとき（[読み直し](conflict-thresholds.md#読み直し)）に読み直す
- **予想するファイル**（`domain::claim_defer::expected_files`）: taskの`--paths`（globのまま）。無ければ`dagq related`で最も似た`completed`のtask 3件の`landed_commits`の各commitが変えたファイル（`git diff --name-only <commit>^ <commit>`）。taskごとにhotspotと同じ間隔でcacheする
- **進行中のrun**（`InFlight`）: `latest_runs_in_progress`（`in_progress`のtaskの最新のrun。着地待ち・`needs_session`を含む）ごとに、base commitからhead（`result_commit`、無ければbranch）までの差分のファイルと、そのtaskの予想するファイル、人だけを待つならその始まり（`owner_waiting_since`。下の「人だけを待つrun」）、自分のcommitを持たないfailedのrunか（`no_commit`。下の「自分のcommitを持たないfailedのrun」）。60秒ごとか、claimの直後に読み直す

### 人だけを待つrun

[ADR-t1484-1](../../adr/2026-10-04-t1484-1-runs-waiting-only-for-a-person-stop-holding-claims-past-a-grace.md)（ADR-0080の決定2・6をamends。task 1484）。進行中のrunのうち次の両方に当たるものは、人の答えだけを待つ（`domain::claim_defer::owner_waiting_since`。supervisorの側は`Supervisor::owner_waiting_since`が`unclosed_run_asks`・`run_lease`・`run_events`から組み立てる。askが無ければleaseとeventは読まない）。

- runに紐づく、答え（`answered_at`）も閉じ（`closed_at`）も無いaskで、kindが`worker_question` / `approve_landing` / `stuck_exit` / `answer_prompt` / `stalled` / `decide`のもの（`waits_for_owner`。`stuck_exit`と`answer_prompt`はtask 1437で新しく開かれなくなり、それより前に開いたaskだけ）がある
- [人の答えを待つrun](waiting.md)の待ち（`WaitState::of`で終わっていない。戻り待ちは動くので除く）にあるか、どのsupervisorのleaseも持たない（`approve_landing`で休む着地待ち、answerを待つ`needs_session`など）。leaseを持ち待ちに居ないrun（作業中・validating・review・着地中・resume中）は、askがあっても動くので除く

待ちの始まりは、待ちならその`run_waiting_started`の時刻、leaseが無いならrunの最後の`lease_acquired` / `lease_released`の時刻で、どちらもそのaskのうち最も古いものの`created_at`より前にはしない（leaseのeventが無ければaskの時刻）。

始まりから`[conflicts]`の`waiting_owner_grace_secs`（既定600秒。[Conflict thresholds](conflict-thresholds.md)）を過ぎたrunは、判定のたびに進行中のrunから除く（`domain::claim_defer::counted`。猶予の経過はcacheの60秒に依らずpassの時刻で判定し、askへの答えや待ちの終わりの反映は進行中のrunの読み直しまで遅れうる）。新しく控えるときも、控えを続けるかの判定でも同じに数え、待ちが終わって動き出したrunは読み直しの後にまた数える

### 自分のcommitを持たないfailedのrun

[ADR-t1634-1](../../adr/2026-10-05-t1634-1-failed-runs-without-their-own-commit-hold-no-claims.md)（ADR-t1484-1の決定2・3をamends。task 1634）。進行中のrunのうち、statusが`failed`（validationで落ち、triage・復旧・askを待つ）で、head（`result_commit`、無ければbranch）がbase commitから何のファイルも変えていない（`changed_paths`が空。headが無いものを含む）runは、猶予を待たずに判定のたびに進行中のrunから除く（`domain::claim_defer::failed_without_commit`が`InFlight`の`no_commit`を決め、`counted`が除く）。validationは落ちたreceiptのcommitも`result_commit`に残すので、`result_commit`の有無ではなく差分で見る。差分が読めなかったrunはwarnを出して数える。自分のcommitを持つfailedのrunと、作業中・着地待ち・`needs_session`のrunは数える。retryで新しいrunがclaimされれば、taskの最新のrunはそれになり、今どおり数える

hotspotが無いか、進行中のrunがどのhotspotも触らなければ、候補のファイルは読まずに全部claimできる。

## 判定

`fill_slots`のclaimのloopで、`dependency_graph`のcandidatesを順に`domain::claim_defer::decide`にかける。

- 候補の予想するファイルと、ある進行中のrunのファイルが同じhotspotを触れば（`glob_matches`）、控える。最初に控えたときだけtaskのevent `claim_deferred`を書く
- 効く優先度が`interrupt`のtaskは控えない。控えていたtaskは`claim_deferral_ended`（`why: cleared`）で終える
- 最初の`claim_deferred`から`[conflicts]`の`defer_max_secs`（既定3600秒。そのpassで使っている値で、読み直した新しい値は進行中の控えにも効く）を過ぎたtaskは、重なっていてもclaimし、`claim_deferral_ended`（`why: expired`）を書く。そのtaskが次にclaimされるまで（重なりが一度消えても）控え直さない。`defer_max_secs`は正の整数で、控えを無効にする値は無い
- 重ならなくなったtaskは`claim_deferral_ended`（`why: cleared`）を書いてclaimする
- 猶予を過ぎた人だけを待つrunと自分のcommitを持たないfailedのrunを除くと重ならなくなったtask（邪魔なrunが全てそれ）は、`defer_max_secs`を待たずに`claim_deferral_ended`を書いてclaimする。`why`は、邪魔なrunが全て自分のcommitを持たないfailedのrunなら`no_commit`、猶予を過ぎた人だけを待つrunが混じれば`owner_waiting`（`domain::claim_defer::left_out`）。控えていなかったtaskは何も書かずにclaimする。そのrunがまた動き出して重なれば、新しく控える（始まりと上限はその`claim_deferred`から）
- 控えていたtaskが候補から外れたら（他のsupervisorがclaimした、cancel、依存が戻った）`claim_deferral_ended`（`why: not_candidate`）を書く

控えたtaskを除いた順で`claim_for_supervisor_in_order`がclaimする。1件claimするたびに進行中のrunを読み直すので、同じpassでclaimしたrunとの重なりも見る。他にclaimできるtaskが無くslotが空いていても控えたまま待つ（直列にする）。上限は上の`defer_max_secs`。

supervisorは控えの状態（taskごとの始まりと上限を過ぎたか）を持ち、起動して最初の判定で、taskごとの最新の`claim_deferred` / `claim_deferral_ended` / `run_claimed`（`latest_task_events`）から組み立て直す。最新が`claim_deferred`なら控えの途中、`claim_deferral_ended`の`why: expired`なら上限を過ぎた控え、それ以外は控えていない。supervisorが入れ替わっても上限は最初の`claim_deferred`から数える。

## workerを動かせないtask

supervisorは、どのproviderでも動かせないworker（providerと経路の組）のtaskをclaimせずに飛ばす（ADR-t813-2。task 814・818）。hotspotの判定より先に、`Queue::candidates`のtaskの`worker`にこのpassの経路（`Supervisor::routes`、`domain::provider_switch::routes`: 自分のadapterの表と2つのproviderの控えから決める。[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)）があるかを見る（全workerに経路があるときは読まない）。そのproviderが使えなくても、もう一方の非対話で動かせるtaskは控えずにそちらでclaimする。

- 理由は`provider_unavailable`（そのproviderが使えず、もう一方でも動かせない）か`mode_unavailable`（providerの組はあるがその経路の組が無い）。最初に飛ばしたときだけ`claim_deferred`（`domain::claim_defer::worker_deferred`）を書く
- 両方のproviderが使えない（Claudeの控えのaskが開き、Codexが無いか控えられている）passは、控えのaskが新しいclaimを止める（`claim_held`）が、その前に候補をこの判定にかけ、`provider_unavailable`の控えを記録する（`status`の`claim_deferrals`に出る）
- 上限は無く、hotspotの控えと違って`defer_max_secs`で期限切れにならない。表にそのworkerが入ったsupervisorは`claim_deferral_ended`（`why: cleared`）を書いてclaimし、候補から外れたtaskは`why: not_candidate`で終える
- 起動して最初の判定で、taskごとの最新のeventがこの理由の`claim_deferred`なら控えの途中として組み立て直す（`worker_deferrals_in_place`）。hotspotの控え（`deferrals_in_place`）は`reason`が`hot_files`のもの（`reason`の無い古いeventを含む）だけを読む
- 同じpassで`claim_for_supervisor_in_order`にも経路の一覧を渡し、順に無いtaskを取る後戻りでも経路の無いworkerのtaskは取らない。経路がもう一方のproviderなら、runはそのproviderの非対話で始まり、`provider_switched`（`phase: start`）を記録する

## 記録

- `claim_deferred`（taskのevent）: `reason: hot_files`、`files`（重なったhotspot）、`runs`（`[{run_id, task_id}]`、重なった進行中のrun）、`max_secs`、`message`、`supervisor`。supervisorのlogにwarnで出る。workerを動かせないtaskの`claim_deferred`は`reason`（`provider_unavailable` / `mode_unavailable`）、`provider`、`worker_mode`、`message`、`supervisor`
- `claim_deferral_ended`（taskのevent）: `reason`、`why`（`cleared` / `owner_waiting` / `no_commit` / `expired` / `not_candidate`）、`deferred_secs`、`supervisor`。logにinfoで出る

## `status`と`stats`

- `status`: `claim_deferrals`に、今控えているtask（taskの最新の`claim_deferred` / `claim_deferral_ended` / `run_claimed`が`claim_deferred`のもの）を`{task_id, reason, since, files, runs, supervisor}`で並べる（[`status`](status.md)）
- `stats`: `claim_deferrals`に、windowの中で始まった控えの`count`と`secs`、終わり方ごとの`by_end: {<why>: {count, secs}}`（`cleared` / `owner_waiting` / `no_commit` / `expired` / `not_candidate`、`claim_deferral_ended`の前にclaimされた`claimed`、まだ終わっていない`open`、同じtaskの`claim_deferred`で置き換わった`superseded`）、hotspotごとの控えた回数`by_file`、今の控え`deferred`を出す。控えは次の`claim_deferral_ended`かそのtaskの`run_claimed`で終わり、まだ終わっていない控えはwindowの終わりまでを数える。空きslotがあり控えているtaskがあれば、alert `claim_deferred`（`value`は控えているtaskの数）を`idle_slots`の代わりに出す（[`stats`](stats.md)）
