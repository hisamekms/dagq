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

Claude Codeのログインが切れるか利用上限に達すると、新しいrunもheadlessのjobも同じ理由で止まる。
runtimeはそれをqueueで1件の`queue_hold`のask（`authentication`、または`cost`で`subject: usage_limit`）にまとめ（下の「検知」と[ask](ask.md)）、openな間は新しいclaimとheadless jobの起動を控え、人のanswerを自分で適用する（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定42の「待ち」と「answer」）。
ディスクの`cost`のaskは[空き容量を確かめる](disk-space.md)が扱い、ここの控えとanswerの適用には入らない。

## 検知

文言の型は`infrastructure::claude`の1か所に置き、turnの失敗とheadless jobの出力をそこで読む。
headless jobの失敗は共通の分類`JobFailure`（[Agent provider lifecycle](../provider-lifecycle.md#headless-jobのinterface)）で返り、ログインの切れと利用上限が壁になる。
壁は`domain::queue_hold::Wall`で、askの理由と`subject`、記録するevent、questionの文面を持つ。

- **worker の turn**: 非対話のturnの壁はproviderが読む。
  使えるもう一方のproviderがあれば次のturnから切り替え、使えなければholdのaskで待つ。
- **headless jobの出力**: 失敗したjob（exit非0・時間切れ・読めないverdict。verdictが読めて適用に失敗したものは読まない）の出力を、起動したproviderが分類する（Codexの失敗はaskに入れない。[Observer](observer.md#codexで動かす)）。
  成功したjobの出力は読まない（作業の中で文言を引用しても壁にしない）。
  見つかればjobをaskに足し、壁のeventをjobのrunに、runの無いjob（plan review・goal review・observer）はqueueに書く。
  askの控えはそのpassから効く（同じpassで後から始まるはずのplan review・goal reviewも始めない）。
  jobごとの扱い:
  - review: `review_failed`も`approve_landing`のaskも作らない。
    始まった後のjobが壁で止まれば、そのjobは終わりを記録して`Phase::Review`の中で待ち、reviewは他のjobの答えを保ったまま進む。
    待つjobは控えが解けたpassで同じproviderで起動し直し、自分の`provider`を書いたjobは切り替えが許されていればもう一方へ進む（[Review](review.md#reviewのprovider)）。
    `affected`に載るのはrunではなくreviewのjobなので、`cancel_affected`でもrunは手放さず、askが閉じた後にそのjobを起動する。
    起動の前に全体のreviewのjobも自分の`provider`を書いたagentのjobも控えで待つreviewは`ReviewHeld`に居て、askを開いたjobを持たず控えを読んで待つだけなので`affected`にrunもjobも載らず、同じく`cancel_affected`でも手放さず、どれかのjobが起動できるpassで始まる
  - 終わったrunの復旧job: `triage_failed`を書くが、askに居る間は人のtriageのattentionにしない。
    `done`が起動し直す
  - 生きているsessionの復旧job: そのalertのask（ADR-t609-1）を開かず、失敗も記録しない。
    控えが解けた後のpassでalertの次のjobが始まる
  - plan review・goal review: 失敗を書くが、askに居る間は人のreviewのattentionにしない。
    `done`が出し直す・rearmする
  - observer: 終わりの記録に壁とaskを足す。
    控えが解けると次の間隔で動く
- **affectedのjob**: askの`affected`にはrunのIDと並べてjobの項目（`domain::queue_hold::HoldJob`）を足す。
  jobの項目は空白を含み、runのIDは含まないので区別できる。
  runの無いjobが足されたときの`ask_updated`はqueueイベント（taskもrunも無い）になる。
  attentionは、閉じていない対象のaskに居るjobの失敗を出さず、ask自体をattentionにする

## providerごとの控え（ADR-t813-2）

[ADR-t813-2](../../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)の決定6（ADR-0047決定42をamends）で、控えの単位はproviderごとである。
このページの`queue_hold`のaskはClaudeの控えで、Codexの控えはaskを開かないqueueイベント（[provider-lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)）。

- **workerのclaimとturn**: 使えるproviderがあれば止めない。
  askが開いていても（Claudeが控えられていても）Codexのworkerが動かせれば、新しいclaimは控えず、taskは非対話のCodexで始まる。
  Codexが控えられていればCodexのtaskは非対話のClaudeで始まる。
  両方が使えない（Claudeのaskが開き、Codexが無いか控えられている）ときだけ、下の「控え」のとおりaskが新しいclaimを止める。
  workerの切り替えを止める設定のときは、Claudeのtaskはもう一方へ移らずClaudeの控えが解けるのを待ち（[Claudeを使わない運転](../provider-lifecycle.md#claude-を使わない運転)の下を除く）、Codexのtaskはそのまま動くのでaskは新しいclaimを止めない（[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)の「切り替えを止める設定」、[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）
- **askを開くとき**: Claudeが止まったとき（headlessのjobの出力、Claudeの非対話のturn、非対話のruntimeのplannerのturn）は、Codexが使えても開く（Claudeで動く役割のjobは人を待つ）。
  Claudeの非対話のturnからCodexへ移ったrunはaskの`affected`に入れず、runに壁のeventを移った先とともに書く。
  非対話のruntimeのplannerのturnがClaudeの壁で止まったときは、runもjobも名指さないaskを開くか開いているaskに加わり（`affected`に入らない）、queueに`provider_waiting`を残す。
  plannerは終わらずに待ち、答えで控えが解けた後のpassで、失敗したturnの依頼を載せたturnで続ける（[`plan` / `planners`](plan-planners.md#runtimeのplannerの経路)の「使えないとき」、ADR-t1394-2決定5）。
  Codexだけが止まったときは開かない。
  止まったrunがもう一方へも移れないときはrunを失敗にせず待たせ、Claudeの認証と利用上限か、両方使えないときは、providerに関わらずrunがaskに入る（壁は開いている控えのaskの理由か、認証・利用上限の側の理由）。
  切り替えの上限でCodexから移れずClaudeが使えるときはaskを開かず、Codexの控えが解けた後に同じthreadへもう一度送る
- **`done`**: 下の適用に加え、askを閉じるsupervisorがCodexの控えも解く（どのsupervisorも毎passでqueueの控えを読み直す）。
  控えのaskに入って待つrunへの「続けて」は、runの今のproviderに行く
- **Claudeで動く役割**: `[roles.*]`に`provider`を書かない役割のjob（runのreviewではagentの表に`provider`を書いたjobを除く）の控えはaskに従う。
  `provider = "codex"`のobserverと復旧jobはClaudeの控えの間もCodexで始まり、Codexの失敗はaskに入れない（[Observer](observer.md#codexで動かす)）。
  Codexが使えずに止まったjobはCodexを控えて次のjobをもう一方で（jobの切り替えを止める設定なら控えが解けた後にCodexで）始め、それ以外の失敗はjobの今の失敗の経路（終わったrunは`triage_failed`、生きているrunはalertのask）に入る（[Triage](triage.md#codexで動かす)）
- **providerを書いた役割のjob**（[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)の決定5がADR-t813-2の決定6をamends。goal review・plan review（[Plan review](plan-review.md#codexで動かす)）・スループットの見直し（[Throughput review](throughput-review.md#codexで動かす)）・observer（[Observer](observer.md#codexで動かす)）・復旧jobとrunのreview（[ADR-t1207-1](../../adr/2026-09-30-t1207-1-codex-run-review.md)））: jobを止めるのは、その役割のproviderと切り替え先（その役割を動かす実装を持つもう一方のprovider）の両方が控えられているか、supervisorに無いときだけ。
  Codexが止まったjobはaskを開かず`ProviderHold`で控え、そのjobをもう一方で始め直す（復旧jobの終わりは人のものにせず、終わったrunは次のラウンド、生きているrunはalertの次のjobをもう一方で始める）。
  Claudeの控えのaskが開いていても、Codexで動けるgoal reviewはCodexで起動し、Codexが控えられていればClaudeで起動する（[Goal review](goal-review.md)の3）。
  Codexのgoal reviewが認証・利用上限・起動の失敗で止まったときはaskを開かず`ProviderHold`で控え、Claudeのgoal reviewが止まったときはaskを開いてjobを`affected`に足す。
  どちらもgoal reviewの行は`interrupted`で閉じ、次のpassでもう一方のproviderで起動し直す（両方控えられていれば起動しない）。
  runのreviewはjobごとに同じく、Codexで止まったjobはaskを開かず`ProviderHold`で控え、そのjobだけを同じpassでもう一方で起動し直し、両方使えなければ他のjobの答えを保ってそのjobが待つ。
  `--no-claude`では切り替え先が無いので待たず、`review_failed`としてleaseを放し`approve_landing`のaskで人を待つ（ADR-t1207-1 決定 4）。
  jobの切り替えを止める設定（[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）のときは、これらのjobはもう一方で起動せず、実際に失敗したproviderの控え（Codexの`ProviderHold`、Claudeの控えのaskか起動の失敗の`ProviderHold`）が解けるのを待って同じproviderで起動する。
  控えと人への届け方は変わらない（[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)の「切り替えを止める設定」）。
  Claudeの控えでaskを開く条件（Claudeでしか動けない役割があること）は変えない。
  runtimeのplanner（と`provider = "codex"`を書かない役割のjob）はClaudeで動くので、Claudeの止まりはaskを開く

## どのaskが控えるか

`domain::queue_hold::reason_of`: kindが`queue_hold`で、理由が`authentication`か、`cost`で`subject`が利用上限のもの。
`subject`が`disk`のものは対象外で、それ以外の`subject`の`cost`は利用上限と同じに扱う。
`hold_of`はそのうちopen（未回答）のものだけを控えにする。

## 控え

supervisorは毎pass（drainの途中も）、ディスクの確認の次に`Supervisor::check_queue_hold`でqueueのcloseされていない対象のaskを読み、回答済みのものを適用してから、openなものの最初の1件を控えとして持つ。
持っている間:

- **claim**: `ClaimHold::judge`は控え（どのworkerにも経路が無いときだけ渡す。上の「providerごとの控え」）をディスクとloadより先に判定し、`claim_held`を記録してclaimしない。
  askが閉じるか回答されると次のpassで`claim_resumed`になる（記録の規則は[claimを控える](claim-hold.md)と同じ）
- **review**: 受理されたrunのreviewは、先頭の[プログラムのreview](review-programs.md)をproviderを使わないので控えの間も流し、それが全部通った後のagentのreviewだけが`Phase::ReviewHeld`で待つ。
  プログラムのreviewで落ちたrunは`ReviewHeld`に入らずworkerに差し戻され、起動の失敗と時間切れはreviewの失敗になる。
  待つ間sessionは開いたまま、`review_started`を書かない。
  reviewが待つのは、全体のreviewのjobと、自分の`provider`を書いた各agentのjobの行き先がどれも待つときだけである。
  `provider`を書いたjobの行き先が待つのは、そのproviderが使えず、jobの切り替えを止める設定か切り替え先も使えないときである。
  始まった後のjobが控えに当たれば、reviewは`ReviewHeld`に入らず、そのjobだけが他のjobの答えを保って待つ（[Review](review.md#reviewのprovider)）。
  そのreviewは`cancel_affected`でも手放さず、控えが解けた後にそのjobを起動し直す。
  控えが解けたpassでagentのreviewが始まり、プログラムのreviewは流し直さない。
  引き継ぎとadoptは`awaiting_integration`のreviewの無いrunを組み立て直すので、プログラムのreviewから流し直し、agentのreviewは同じく待つ
- **復旧job**: `provider`を書かない復旧jobは、終わったrunのtriageを始めない。
  生きているsessionのalertの復旧jobも始めず、alertは控えが解けた後のpassでまた拾う（長く走るbackgroundのalertは、見た印を進める前に判定するので失われない）。
  `provider = "codex"`の復旧jobは上の「providerを書いた役割のjob」のとおり控えの間もCodexで始まる
- **plan review・goal review・observer**: plan reviewは起動しない。
  goal review・observerは`provider`を書かない役割を控えの間は起動しない。
  走っているjobは最後まで追う。
  `provider`を書いたgoal review・observerは、上の「providerを書いた役割のjob」のとおり控えの間も使えるproviderで起動する
- 走っているrunはleaseとsessionを持ったまま進む。
  着地（`integrate`）はClaudeを使わないので控えない

`queue_hold`のaskに入った（`affected`に居る）runでは促しとstalledのaskが止まり、待ちから`queue_hold`で戻る（[人の答えを待つrun](waiting.md)）。
促しを止めるのはaskが閉じるまで（answerの後、supervisorが`done`を適用して続けてよいという文を打つまでの間も含む）で、その間に促しが先に打たれることはない。

## answerの適用

answerがaskのoptionsのどれかなら、runtimeが届ける答えとして記録し、`status`はattentionを答えを適用中として人の操作を求めない。
ディスクのaskの答えも同じ（supervisorが適用する）。
optionsに無い自由な答えはinboxが人に見せる。
回答済みのaskは控えない。

答えは、runを持つsupervisorがそれぞれ自分のslotのrunに適用する。
どのsupervisorも毎passで、回答済みで閉じていない対象のaskを読み（`Supervisor::apply_hold_answer`）、`affected`のrun（jobの項目は除く）のうち自分のslotに居て、まだそのaskの`hold_answer_applied`が無いものに答えを適用し、runに`hold_answer_applied`を書く。
印は答えが働く前に書く（手放したrunはleaseを持たないので、印より先に手放すと、同時にaskを閉じる別のsupervisorが`unwatched`と読んでしまう）。
この印が1つのrunに答えを1回だけ適用させる（次のpassも、runを引き継いだ・adoptした別のsupervisorも、印のあるrunには適用しない）ので、`done`の文面の二重送信や二重のabandonは無い。

- **`done`**（`continued`）: 生きているsessionを見ている段（`Session` / `Revise` / `Resume`）に居るrunに、そのsessionの監視（`SessionWatch::continue_after_hold`）が決まった文面`queue_hold::CONTINUE_TEXT`を1回送る。
  workerのsessionではwrapperが生きていて終了の依頼の前、receiptが無いとき、reviseとresumeでは答えの配送と同じ位置で送り、送った後はその段の時間切れを数え直す。
  送信は次のturnの依頼としてrun dirの`turns/`に書く。
  送ったらrunに`hold_continue_sent`を書き、stallの計時は送った時刻からやり直す。
  送れなかったらlogだけ残し、stallの促しが続きを頼む。
  receiptの後やreview・exitの段に進んだrunには送らない（`moved_on`）
- **`cancel_affected`**（`released`）: 生きているsessionの段かreviewの待ち（`ReviewHeld`）に居るrunを、slotから外してheadless jobを止め、`abandon`と同じく手放す: `runtime_error`（コード`hold_canceled`）を書いてleaseを返す。
  statusは変えず、sessionとworktreeは残る。
  inboxには`recover run`のattentionとして出る（成果を捨てるかどうかは、この答えを選んだ人の判断で、`recover`の手順で決める）。
  自分で先へ進んだrun（validation・review・着地の途中）はもう止まっていないので手放さない（`moved_on`）。
  答えを当てるのは`affected`に載ったrunだけなので、`affected`に載らずに`ReviewHeld`で待つrun（起動の前にどのjobの行き先も控えを読んで待つreview）は手放さず、どれかのjobが起動できるpassでreviewを始める

### askを閉じる

適用の後、そのsupervisorはaskを閉じられるかを見る（`Supervisor::close_hold`）。
`affected`のrunがどれも次のどれかなら閉じる:

- `hold_answer_applied`がある
- leaseが無い（`unwatched`: 終わったか手放されたrunで、答えを適用するsessionが無い）
- 答えから待つ時間が過ぎた。
  そのとき別の生きているsupervisorがleaseを持つのに適用していないrunは`elsewhere`（そのsupervisorが答えを読めない古いバイナリか、passが止まっている）、leaseがstaleなrunと、このsupervisorのleaseなのにslotに無いrunは`unwatched`になる

runのeventかleaseが読めなかったpassは閉じず、次のpassで読み直す。
引き継ぎ（exec）の後のsupervisorは、runのslotを組み立て直すまで答えを適用しない（組み立て直す前のpassは控えを読むだけ）

別の生きているsupervisorがleaseを持つrunが適用されていない間は閉じず、そのsupervisorの次のpassが適用する。
leaseがstaleなrunも待つ時間までは待つ（生きているsupervisorがadoptすれば、自分のslotのrunとして適用する）。
閉じていない間はそのrunの促しとstalledのaskを止めたままなので、`done`の文面より先に促しが打たれることはない。
回答済みのaskは控えないので、claimとheadless jobは答えた時点で再開する。

閉じたsupervisorだけが（askを閉じるのに負けたsupervisorは何もしない）、`done`なら失敗したjobを起動し直し、queueに`queue_hold_applied`を書く。
`done`で起動し直すのは、askが開く少し前（askはsessionかjobがエラーを見せてから開くので、その直前に記録された失敗を含める）以後に失敗したjob（`Supervisor::restart_failed_jobs`）:

- 終わったrunの復旧job: runが`failed` / `interrupted`でleaseが無く、triageの最新が失敗なら、runに`job_restarted`を書き、次のpassのtriageが拾う（試行回数は数え続け、上限を超えればaskになる）
- plan review: proposalがまだ失敗で止まっていれば、`submit --proposal ID`と同じく出し直す
- goal review: goalがまだ失敗で止まっていれば、`goal review ID`と同じくrearmする
- 起動し直さないもの: 壁で止まったreviewのjobはreviewの中で待っているので、控えが解けたpassで自分で起動し直す（`ReviewHeld`で待つreviewも、どれかのjobが起動できるpassで自分で始まる）。
  それ以外のrunのreviewの失敗は失敗のときに`approve_landing`のaskを開いているので人の答えに任せる。
  生きているsessionの復旧jobの失敗はそのalertのask（ADR-t609-1）になり人の答えに任せ、askはsessionが進めば閉じる（壁で止まったものはaskにせず、控えが解けた後のpassで次のjobが始まる）。
  observerは次の間隔で動く

`cancel_affected`はjobを起動し直さず、askが閉じるので、失敗したjobはそれぞれの人のattentionに戻る。
reviewの中で待つjobと`ReviewHeld`で待つreviewは、控えが解けたpassで自分で起動する。
`elsewhere` / `unwatched`に載ったrunには答えが適用されていないので、`cancel_affected`ならinboxが`queue_hold_applied`を見て`recover`で手放し、`done`ならそのrunのstallの促しが続きを頼む。
引き継ぎ（exec）の後のsupervisorは、runを組み立て直す前に控えを読むので、待っていたreviewを始めない。
