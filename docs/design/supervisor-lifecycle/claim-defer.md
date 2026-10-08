---
id: design-supervisor-lifecycle-claim-defer
type: design
title: "claimを控える（衝突の多いファイル）"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-0080
  - adr-t1484-1
  - adr-t1981-1
  - adr-t1634-1
  - adr-t1632-1
  - adr-t813-2
  - adr-t1857-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-claim-hold
  - design-supervisor-lifecycle-conflict-thresholds
  - design-supervisor-lifecycle-status
  - design-supervisor-lifecycle-stats
---

# claimを控える（衝突の多いファイル）

候補のtaskが触ると予想するファイルと、進行中のrunのファイルが、衝突の多いファイル（hotspot）で重なるとき、supervisorはそのpassでそのtaskをclaimせずに次の候補をclaimする（[ADR-0080](../../adr/0080-supervisor-rereads-conflicts-config.md)、ADR-0069を統合。goal 45、task 463・585）。queue全体のclaimを止める[load averageの控え](claim-hold.md)とは別で、1つのtaskだけを飛ばす。判定は`domain::claim_defer`、supervisorの側は`application/supervise/claim_defer.rs`（`Supervisor::claimable`）。同じ候補のloopで、[workerを動かせないtask](#workerを動かせないtask)と[依存先の着地を含むbuildを待つtask](#依存先の着地を含むbuildを待つtask)も1つのtaskを飛ばし、同じeventで記録する。

## 判定の入力

- **hotspot**: `stats`の`conflict_hotspots`（既定のwindow）で`alert`のファイル（mainから消えたものを除き、名前が変わったものは今の名前）。閾値は`dagq.toml`の`[conflicts]`（[Conflict thresholds](conflict-thresholds.md)）。plan reviewのpromptと同じ計算（`Supervisor::conflict_hotspot_files`）で、10分ごとと、`[conflicts]`の値が変わったとき（[読み直し](conflict-thresholds.md#読み直し)）に読み直す
- **予想するファイル**（`domain::claim_defer::expected_files`）: taskの`--paths`のうちワイルドカード（`*`・`**`・`?`）を含まない具体的なパス（globは範囲の宣言で予想ではないので外す。[ADR-t1981-1](../../adr/2026-10-07-t1981-1-expected-files-leave-out-path-globs.md)、ADR-0080の決定1をamends）。具体的なパスが残らなければ（`--paths`が無いときと同じく）`dagq related`で最も似た`completed`のtask 3件の`landed_commits`の各commitが変えたファイル（`git diff --name-only <commit>^ <commit>`）。taskごとにhotspotと同じ間隔でcacheする
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

supervisorは、どのproviderでも動かせないworker（providerと経路の組）のtaskをclaimせずに飛ばす（ADR-t813-2。task 814・818）。hotspotの判定より先に、`Queue::candidates`のtaskの`worker`にこのpassの経路（`Supervisor::routes`、`domain::provider_switch::routes`: 自分のadapterの表と2つのproviderの控えから決める。[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)）があるかを見る（全workerに経路があるときは読まない）。そのproviderが使えなくても、もう一方の非対話で動かせるtaskは控えずにそちらでclaimする。ただし`dagq.toml`の`[provider_fallback] workers = false`のときは、頼んだproviderが使えない（実行ファイルが無い・起動できない・認証・利用上限）taskはもう一方で動かせても経路が無く、控えて頼んだproviderの控えが解けるのを待つ（実行ファイルが無いときは解ける控えが無く、その実行ファイルを持つsupervisorが動くまで控えたまま。[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）。例外は`--no-claude`で、Claudeを頼んだtaskはoffでもCodexでclaimする（[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)の「切り替えを止める設定」）。

- 理由は`provider_unavailable`（そのproviderが使えず、もう一方でも動かせない）か`mode_unavailable`（providerの組はあるがその経路の組が無い）か`provider_fallback_off`（そのproviderが使えず、もう一方では動かせるが`[provider_fallback] workers`がfalse）。最初に飛ばしたときだけ`claim_deferred`（`domain::claim_defer::worker_deferred`）を書く
- 両方のproviderが使えない（Claudeの控えのaskが開き、Codexが無いか控えられている）passは、控えのaskが新しいclaimを止める（`claim_held`）が、その前に候補をこの判定にかけ、`provider_unavailable`の控えを記録する（`status`の`claim_deferrals`に出る）
- 上限は無く、hotspotの控えと違って`defer_max_secs`で期限切れにならない。表にそのworkerが入ったsupervisorは`claim_deferral_ended`（`why: cleared`）を書いてclaimし、候補から外れたtaskは`why: not_candidate`で終える
- 起動して最初の判定で、taskごとの最新のeventがこの理由の`claim_deferred`なら控えの途中として組み立て直す（`worker_deferrals_in_place`）。hotspotの控え（`deferrals_in_place`）は`reason`が`hot_files`のもの（`reason`の無い古いeventを含む）だけを読む
- 同じpassで`claim_for_supervisor_in_order`にも経路の一覧を渡し、順に無いtaskを取る後戻りでも経路の無いworkerのtaskは取らない。経路がもう一方のproviderなら、runはそのproviderの非対話で始まり、`provider_switched`（`phase: start`）を記録する

## 依存先の着地を含むbuildを待つtask

supervisorは、`wait_for_build`を宣言したtask（`add --wait-for-build`。[Domain model](../domain-model.md)の`Task`）を、自分のbuild識別子のcommitが依存先の着地commitを全て含むまでclaimせずに飛ばす（[ADR-t1632-1](../../adr/2026-10-05-t1632-1-claim-waits-for-a-build-that-contains-the-dependencies-landings.md)。task 1632）。判定は`domain::build_wait`、supervisorの側は`Supervisor::lacking_from_build`。

- **入力**: passごとに`TaskStore::build_waits`が、`ready`で`wait_for_build`のtaskごとに、直接の依存先（`task_dependencies`）の着地commit（`landed_commits`。`run_integrated`の`result_commit`）を1回のqueryで読む（宣言したtaskが無ければ空）。依存先が何も着地させていなければ待つものは無い。build識別子はsupervisorの`Layout::version`（`dagq::VERSION`、[Build identifier](build-identifier.md)。testは`SuperviseOptions::build`で差し替える）で、そのcommit（共有の`build_id::named_commit`）と着地commitを`git merge-base --is-ancestor <着地> <buildのcommit>`（`Repository::is_ancestor`）で比べる
- **判定の順**: workerの判定の後、hotspotの判定より前。効く優先度が`interrupt`でも待つ（宣言は落ちる関門を避けるためで、急ぎでも同じに落ちる）。待つtaskはhotspotの判定にかけない
- **判定**（`build_wait::judge`の`Verdict`。build識別子のcommitは共有の`build_id::named_commit`）: 全てを含めば`Contains`でclaimへ進む。1つでも含まない（`Lacks`）か、含むかを読めない（gitのerror。passごとに問い直し、warnは着地commitごとに1回）ならclaimしない。`.dirty`のbuildはそのcommitで判定する（そのcommitからlocalの変更を足して作ったbuildなので）。commitを名乗らないbuild（リリースの`X.Y.Z`、`+unknown`）は判定できず（`Unjudged`）、待たずにclaimし、taskごとに1回warnを出す（来ないかもしれないbuildを待ち続けないため）。verdictからclaimするか・待つ着地・warnを出すかを決めるのは純粋関数`build_wait::claim_step`
- 理由は`not_in_build`。最初に飛ばしたときだけ`claim_deferred`（`build_wait::deferred`）を書く
- 上限は無く、hotspotの控えと違って`defer_max_secs`で期限切れにならない。自動更新はruntimeのpath（`RUNTIME_PATHS`）を変える着地でだけbuildする（[Auto-update](auto-update.md)の「きっかけ」）ので、依存先の着地がruntimeのpathを変えなければ、その着地を含むbuildは後でruntimeを変える別の着地のbuildが引き継ぐまで来ず、それまで待つ。含むbuildのsupervisorは`claim_deferral_ended`（`why: cleared`）を書いてclaimする。宣言を外したtask（`edit --no-wait-for-build`。draft / submittedに戻してから）も、候補に戻ったときに控えの途中なら`cleared`で終える。候補から外れたtaskは`why: not_candidate`で終える
- **見直し**: 含むかの答えは着地commitごとにprocessの中でcacheする（`DeferWatch::in_build`）。build識別子はprocessの間変わらず、変わるのは自動更新（[Auto-update](auto-update.md)）や`install`の引き継ぎ（[Handoff](handoff.md)）でexecした新しいprocessだけなので、cacheは`supervisor_handed_off`の後に空から始まり、その最初のpassで全ての待ちを新しいbuildで判定し直す。`update_installed`が固定バイナリを入れ替えても、引き継がなかったsupervisor（`up --auto-update`でない、引き継ぎに失敗した）のbuildは変わらず、待ちも変わらない
- **claimの後戻り**: `claim_for_supervisor_in_order`は、渡した順のtaskがどれもreadyでなくなった（他のsupervisorがclaimした、cancel）ときにclaimの順へ後戻りするが、そこでは`wait_for_build`のtaskを取らない（取れるtaskが他に無ければ`NoReadyTask`。`claim_task`）。そのtaskは判定したsupervisorが順に名指したときだけclaimされる。順を渡さない`TaskStore::claim`も同じで取らない。hotspotで控えたtaskは後戻りで取られうる（従来どおり）
- **重なり**（1つの候補の待ちの始まり・続き・終わりと他の控えとの重なりは純粋関数`build_wait::decide`、候補から外れた待ちの`not_candidate`は`build_wait::left`）: taskの最新の控えのeventが今の控えを表すように、workerの控えを`cleared`で終えたtaskがまだbuildを待てば、buildの`claim_deferred`を書き直す。hotspotで控えている途中（期限切れでない）のtaskがbuildを待ち始めたら、hotspotの控えはbuildの`claim_deferred`に置き換わり（`stats`の`superseded`）、buildが含んだ後に重なりが残ればhotspotの判定が新しく控える
- 起動して最初の判定で、taskごとの最新のeventがこの理由の`claim_deferred`なら控えの途中として組み立て直す（`build_wait::deferrals_in_place`）。`worker_deferrals_in_place`はこの理由を読まない

## 記録

- `claim_deferred`（taskのevent）: `reason: hot_files`、`files`（重なったhotspot）、`runs`（`[{run_id, task_id}]`、重なった進行中のrun）、`max_secs`、`message`、`supervisor`。supervisorのlogにwarnで出る。workerを動かせないtaskの`claim_deferred`は`reason`（`provider_unavailable` / `mode_unavailable` / `provider_fallback_off`）、`provider`、`worker_mode`、`message`、`supervisor`。`provider_fallback_off`の`message`は、頼んだproviderが今使えず`[provider_fallback] workers`がfalseなのでもう一方のproviderで始めず、そのproviderが使えるようになるまでclaimしないと言う。buildを待つtaskの`claim_deferred`は`reason: not_in_build`、`build`（そのsupervisorのbuild識別子）、`missing`（`[{task_id, commit}]`、含まない依存先の着地）、`message`、`supervisor`
- `claim_deferral_ended`（taskのevent）: `reason`、`why`（`cleared` / `owner_waiting` / `no_commit` / `expired` / `not_candidate`。`owner_waiting` / `no_commit` / `expired`はhotspotの控えだけ、workerとbuildの控えは`cleared` / `not_candidate`だけ）、`deferred_secs`、`supervisor`。logにinfoで出る
- 2つのeventはclaimの前の待ちのうちtaskの控えの区間の始まりと終わりを兼ねる（[計測](../measurement.md)の「claimの前」）。
  区間は下の`stats`の`by_end`と同じ規則（`domain::claim_defer::deferral_spans`）で、どのsupervisorの記録でも終わり、終わりを書いたsupervisorを`ended_by`に残す。

## `status`と`stats`

- `status`: `claim_deferrals`に、今控えているtask（taskの最新の`claim_deferred` / `claim_deferral_ended` / `run_claimed`が`claim_deferred`のもの）を`{task_id, reason, since, files, runs, supervisor}`で並べる。buildを待つtask（`reason: not_in_build`）は`files` / `runs`がnullで、`build`と`missing`を足す（他の理由では出さない。`domain::claim_defer::OpenDeferral`。[`status`](status.md)）
- `show`: 既定の（圧縮した）出力は、今控えているtaskなら同じ形を`claim_deferral`に出す（控えていなければこのキーは無い。`view::task_detail`）。`show --full`は`TaskDetail`をそのまま出すのでこのキーを持たず、控えはeventsの`claim_deferred` / `claim_deferral_ended`で読む。`wait_for_build`を宣言したtaskはtaskの`wait_for_build: true`でも分かる
- `candidates`と`graph`: 同じ記録を`application::claim_view`で読み、開いている控えのtaskを`candidates`から除いて`deferred`に出す（[ADR-t1992-1](../../adr/2026-10-07-t1992-1-candidates-show-the-order-as-claimed-from-the-supervisor-s-deferral-records.md)）。
  CLIは控えを判定し直さず（`decide`・hotspotの計算・providerの経路を走らせない）、記録を閉じたり足したりもしない。
  `held`はclaimを止める条件を順に混ぜずに出す（`application::health::claim_holds`）: 生きたsupervisorが居なければ`no_supervisor`、生きたsupervisor（か名の無いもの）の最新の記録が保留を示す`claim_held`・`run_env_program_missing`、生きたsupervisorごとに自分の最新の記録が保留を示す`ci_watch_held`・`landing_branch_unresolved`、待っているclaim spacing。
  CLIはこれらの条件も判定し直さず、supervisorが記録しない条件は出さない。
  控えが閉じるのはsupervisorが次に判定したときだけで、`fill_slots`はslotが無いpassとclaimを止めているpass（両方のproviderが使えないpassを除く）では判定せず、hotspotの入力もcacheするので、その間は前の判定の控えが開いたまま出る（遅れの上限は無い）。
- `stats`: `claim_deferrals`に、windowの中で始まった控えの`count`と`secs`、終わり方ごとの`by_end: {<why>: {count, secs}}`（`cleared` / `owner_waiting` / `no_commit` / `expired` / `not_candidate`、`claim_deferral_ended`の前にclaimされた`claimed`、まだ終わっていない`open`、同じtaskの`claim_deferred`で置き換わった`superseded`）、hotspotごとの控えた回数`by_file`、今の控え`deferred`を出す。控えは次の`claim_deferral_ended`かそのtaskの`run_claimed`で終わり、まだ終わっていない控えはwindowの終わりまでを数える。空きslotがあり控えているtaskがあれば、alert `claim_deferred`（`value`は控えているtaskの数）を`idle_slots`の代わりに出す（[`stats`](stats.md)）
