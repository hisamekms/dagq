---
id: design-supervisor-lifecycle-up-down
type: design
title: "`up` / `down`"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1582-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-integrate
  - design-supervisor-lifecycle-auto-update
  - design-supervisor-lifecycle-source-repository
  - adr-t614-1
  - adr-t617-2
  - design-plugin-integration
  - adr-0013
  - adr-0011
  - adr-t1433-4
  - adr-0014
  - adr-0045
  - adr-0031
  - adr-t632-1
  - design-provider-lifecycle
  - design-persistence
  - adr-t1228-2
  - adr-t2159-1
---

# `up` / `down`
`dagq up`はqueueのruntimeをcold startする1コマンドで、application層の`src/application/lifecycle.rs`の`up`が行う（`down`も同じファイル。[ADR-0013](../../adr/0013-layered-architecture-and-type-function-style.md)）。
外への口は`lifecycle::Ports`にまとめ、`compose::up` / `compose::down`が組み立てる。
冪等で、続けて2回叩けば2回目は`reused`になる。
外部（launchctl、PIDの生存とsignal）は`LaunchAgent`、`ProcessControl`のtrait越しに呼び、判定はfakeで、実launchdの`up → status → down --wait`はe2eで確かめる（e2eの条件は`tests/e2e.rs`の冒頭のdoc comment）。
in-cmux modeの起動の指定は何にも触る前に拒み、廃止と次の手順を文面に書く（[ADR-t1433-4](../../adr/2026-10-03-t1433-4-supervisor-resides-without-cmux.md)）。

1. **preflight**: 何も起動する前に次を確かめ、だめなら理由と案内を付けたerrorでsessionもsupervisorも開かずに止まる。
   - queueが`init`済みであること（queueはDBの指定かcwdのrepositoryから解決する）と、repositoryのroot。
   - Claudeと、inboxとruntimeのplannerが読むinstall済みの`claude-dagq`が有効なこと（`lifecycle::require_agent`で、`dagq inbox`も同じ判定を行う。[ADR-t617-2](../../adr/2026-09-27-t617-2-installed-plugin-by-default-plugin-dir-for-development.md)の決定4）。
     確かめないときと確かめ方・文言は[plugin integration](../plugin-integration.md#installしたpluginの確認)と`UpEnvironment`のdoc commentにある。
     `dagq plan`は何も開かずに案内を付けて拒むので（[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)）、この確かめを行わない。
   - Claude Codeがrepository rootを信頼済みであること。
     run worktreeの信頼はrepository rootから決まるので、未信頼のまま流すと全runがtrust dialogで止まる。
     未信頼なら、rootで`claude`を一度起動して承認する案内のerrorで止まり、configを書き換えて信頼を代行することはしない（[provider-lifecycle](../provider-lifecycle.md#trust-prompt)）。
   - main checkoutの`dagq.toml`の`[run.env]`が名指すプログラムが`up`のPATHで解決できること（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定9、[Run environment](run-environment.md#runenvが名指すプログラムの検査)）。
   - trustを確かめるrepository rootと、`[run.env]`・言語・着地先のbranchを読む`dagq.toml`は、repositoryのmain checkout（main worktree。[Run environment](run-environment.md#main-checkoutの決め方)）のもの。
     main checkoutが無いrepository（bare、またはlinked worktreeから打った`--separate-git-dir`）では、理由と案内をつけて止まる。
   - 着地先のbranchの解決（`dagq.toml`の`[repository]`か推定）もpreflightで行い、解決できないときと、pushを無効にしていない（`push = false`でない）のに明示した`remote`が無いときは止まる（[landing branch](integrate.md#landing-branch)）。
   - 自動更新を頼んだ`up`は、着地先のbranchの解決の後で、queueのrepository（main checkout）がdagqのソースかを確かめ、ソースでなければsupervisorを起動も引き継ぎもせずに止まる（自動更新はrepositoryのソースからdagqをbuildするため。[ADR-t614-1](../../adr/2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)、[Auto-update](auto-update.md#dagqのソースのrepositoryだけ)、[Source repository](source-repository.md)）。
2. **stale登録の削除**: `supervisors`表のうちPIDが死んでいる行を消し、結果に出す。
   PIDが生きていてもheartbeatの古い行は、そのPIDを登録したプロセスでないと分かれば同じく消す（判定は`pid_taken_over`のdoc comment）。
   `run_leases`は触らない（そのrunの復旧は`doctor` / `recover`の仕事）。
   登録より前に始まったプロセスがPIDにいてheartbeatの古い登録（hang）は消さず、reuseもしない。
3. **supervisor**: 生きていてheartbeatが新しい登録があり、その`binary_version`が全部`up`自身のbuild識別子（[Build identifier](build-identifier.md#build-identifier)）と文字列として同じで、launchd modeの登録があれば読めるplistに`up`の書く`ProcessType`があるならreuseし、plistにもlaunchctlにも触らない。
   1つでもversionが違うか、launchd modeの登録の読めるplistに`ProcessType`が無ければ**入れ替える**（下記）。
   `ProcessType`はlaunchdがagentを読むときだけ効き、`install`と自動更新は引き継ぎだけでplistを読み直させないので、前のbinaryが書いたplistのsupervisorは、自動更新の後も人かinboxが一度`up --auto-update`を打ってdrainと書き直しをさせるまで制限の下で流れる（判定は`agent_lacks_process_type`のdoc comment）。
   liveな登録が無ければlaunchd modeで起動する。
   - **同じqueueの別のlabelのagent**: reuse・引き継ぎ・入れ替え・起動を決める前に、このqueueのDBを指す別のlabelのLaunchAgentを探し（`lifecycle::stray_agents`）、`down`と同じく外して、そのsupervisorはreuseせずにdrainを待つ（`remove_stray_agents_for_up`）。
     別のqueueのDBを指すagentには触れない。
     待つ相手と待たない指定での拒み方は、そのdoc commentと`stray_supervisors`のdoc commentにある。
   - **launchd mode**: LaunchAgentを書いて起動し、登録が現れるまで待つ。
   - **登録済みのin-cmuxのsupervisor**（ADR-t1433-4決定3）: `up`は新しくin-cmux modeで起動せず、登録にも`session_workspaces`にもin-cmuxの記録を書かない。
     読むのは前のbinaryが残した登録だけで、引き継ぎ（下記）では同じプロセスのまま続け、drainと`down`ではSIGINTを送る。
     そのworkspaceはdagqが閉じず人が閉じ、`session_workspaces`の`supervisor`行は消す（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定3・6）。
     launchdに移すのは人かinboxの一度の`down --wait`と、in-cmuxの指定を外した`up`。
     起動するsupervisorのmodeは常に`launchd`で、supervisorの起動の印の`mode`になる（[変更の印](commands.md)）。
   - **待つ相手**: `up`が起動したsupervisorの登録だけを受け取り、既に表にあった登録を取り違えない（`started_registration`のdoc comment）。
   - **modeの記録**: `supervise`は受けた`--mode`を、登録を作った直後に自分の`supervisors`の行の`mode`へ書く。
     LaunchAgentの引数が`--mode launchd`を持つので、`up`を通さずlaunchdが起動したsupervisor（ログイン時・再起動後・`KeepAlive`の起動し直し）もlaunchd modeの登録になる。
     `up`も起動したsupervisorが登録に現れた直後に同じ`mode`を書く。
     `--mode`の無い`supervise`（手起動）の登録は`mode`を持たず、handoffで引き継いだ登録は前の`mode`を保つ。
     modeはプロセスの性質なので登録行と寿命を共にし、supervisorがgracefulに終われば行ごと消える（queue dirのsidecar fileにしなかった理由は[persistence](../persistence.md)のRuntime ownership）。
     `down`はこれを見てSIGTERM（launchd mode / 手起動）とSIGINT（登録済みのin-cmux）を使い分ける。
   - **並列数・待ちの上限・runtimeのplannerの上限・claimを控えるload**（[ADR-0062](../../adr/0062-runs-waiting-for-a-person-leave-the-slot.md)の決定7、ADR-0041の決定12、[claimを控える](claim-hold.md)）: 明示されたときだけsupervisorの引数に足し、無ければsupervisorがmain checkoutの`dagq.toml`の`[supervisor]`かhostから決める（`up`のhelpと`supervise_arguments`のdoc comment）。
     supervisorが値と出どころを登録に書く（[人の答えを待つrun](waiting.md)）。
   - **自動更新**（[ADR-0045](../../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定17）: 自動更新は`up`の指定（`--auto-update`）のときだけ有効で、指定した`up`が起動するsupervisorは自動更新付きで起動し、reuseや引き継ぎで残す登録にも`up`の指定をそろえて書く（書く先は`set_auto_update`のdoc comment、[Auto-update](auto-update.md#auto-update)）。
   - **buildの違うsupervisorの入れ替え**（[ADR-0045](../../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定10・12・15・16）: 登録の`binary_version`は`supervise`プロセス自身が書いた自分のbuild識別子で、`up`はliveな登録のどれか1つでも自分のbuild識別子と違えば（nullを含む。package versionが同じでもcommitやdirtyが違えば違う）入れ替える。
     liveな登録が全部引き継ぎを受けられるなら**引き継ぎ**、そうでなければ**drain**で入れ替える。
     launchd modeの登録は、LaunchAgentの`ProgramArguments`の先頭が`up`自身の絶対pathで、`ProcessType`が`up`の書く値のときだけ引き継ぐ（execはplistもlaunchdがprocessに当てる制限も変えないので、pathの違うagentはlaunchdの次の再起動で前のbinaryに戻り、`ProcessType`の無いagentは制限の下で流れ続ける。そのときはdrainし、`up`がplistを書き直してlaunchdが読み直す。判定は`agent_takes_handoff`のdoc comment）。
     - **migrate**: `up`はまずqueueの未適用のmigrationを数え、全部が互換なら適用して結果に載せる。
       非互換が1つでもあれば、何も起動もsignalもせず、`down --wait` → `dagq migrate` → `up`、または`dagq install --allow-breaking`を案内するerrorで止まる。
       互換のmigrationの後も、古いbinaryのsupervisorとrunのwrapperはfloor以上なのでそのまま動く（[persistence](../persistence.md#database-setup-and-migrations)）。
     - **引き継ぎ**（[Handoff](handoff.md#handoff)）: liveな登録それぞれに`up`自身の絶対pathへのexecを頼み、各登録が同じpidのまま`up`のbuild識別子で戻るのを待つ（`lifecycle::hand_off`）。
       何もsignalせず、LaunchAgentに触らず、走っているrunのsessionもleaseもそのまま。
       待ちの上限は全員で1つで、supervisorは進行中の検証と着地の区切りまで待ってからexecするので、integrateの検証1回分に及ぶことがある。
       戻ったと見なす条件・失敗の条件・猶予の見直し・要求の取り消しは`hand_off`と`HANDOFF_GRACE`のdoc commentと[Handoff](handoff.md#handoff)にある。
       1つの失敗で待ちを止めず、全員の成功か失敗を待つ（[ADR-t632-1](../../adr/2026-09-27-t632-1-handoff-restores-only-when-every-supervisor-failed.md)）。
       1つでも失敗があっても、`up`はそこで止まらず、残りの段を最後まで行ってから失敗する（出力と失敗したsupervisorのその後は`lifecycle::PartialHandoff`のdoc comment）。
       fileを差し替えていないので戻すものは無い。
       launchdへ移すときは`down --wait`と`up`。
     - **drain**（引き継ぎを受けられない前のbinaryの登録、またはpathの違うlaunchd agentの登録）: live全部をdrainしてから1つ起動し直す。
       停止は`down --wait`と同じ順序で、手順は`replace_supervisors`と`drain_supervisors`のdoc commentにある。
       それからlaunchd modeで起動する（入れ替えられる側がin-cmuxでも同じ）。
       待つ相手の除外集合はdrainの後に取り直すので、生き残った「生きているが黙っている」登録を自分の起動したものと取り違えない。
       drainに上限は置かない（runはClaude sessionなので待ち時間はrunの長さそのもの）。
       待たない指定は逃げ道で、入れ替える登録がleaseを持つrunが1件でもあれば、件数とrun idと状態を挙げたerrorで止まる（数えるrunと数えないrunは`leased_runs`と`no_wait_refusal`のdoc comment）。
       判定はsignalもuninstallも何もする前に行うので、止まったときの状態は`up`を打つ前と同じ（PIDの死んだ登録のprune（2）だけは済んでいる）。
       leaseを持つrunが無ければそのまま入れ替えるが、待たない指定ではdrainの待ちにも上限が付き、超えたら残っているtokenとpidを挙げたerrorで止める（停止は既に頼んであり、agentも外れているので、`status`から消えたら`up`を打ち直す）。
   - **入れ替えの対象はliveな登録だけ**: 「生きているが黙っている」古いbinaryのsupervisorは入れ替えられず、`up`はその隣に新しいversionのsupervisorを立てる。
     `status`がstaleとして報告し、同じqueueに生きているsupervisorが他に居ないときだけ、人が`down --force`で止めてから`up`をやり直す。
     生きているsupervisorが居るときは、`down`がその生きている方も止めるので使わず、人がstaleなsupervisorのプロセスだけを止め、残った登録は生きているsupervisorが片付ける（案内は`status`のattentionの`supervisor_stale`の次の手）。
     （[ADR-0014](../../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)のConsequences、[ADR-0073](../../adr/0073-kind-additions-are-compatible.md)の決定16と[ADR-t3134-1](../../adr/2026-10-23-t3134-1-stop-a-hung-supervisor-beside-a-live-one-without-down.md)）
   - **二重のsupervisorの警告**: 同じqueueに引き継ぎの組でないliveな登録が2つ以上あるか、queueのLaunchAgent（自分のlabelと、同じDBの別のlabel）が2つ以上loadされていれば、`doctor`と`status`が同じ判定（`health::duplicate_supervisors`）でwarningを出し、`status`はinboxのattentionにも載せる。
     引き継ぎの元の登録とexecが同じpidで作り直した後継の登録は1つのsupervisorと数えて知らせない（意味と直し方は判定のdoc comment）。
   - plist: `~/Library/LaunchAgents/com.dagq.<queue hash>.plist`で、queue hashの決め方は`QueueLocation::hash`のdoc comment、中身は`src/application/lifecycle.rs`の`LaunchAgentSpec::xml`と`supervise_arguments`と`SupervisorEnvironment`のdoc commentにある。
     `ProcessType`を書くのは、無いとlaunchdがsupervisorとその子（着地の検証・worker・review）のCPUとI/Oを絞り、着地の検証が遅くなるからである。
     `up`が書くplistに`--cmux`は無く、前のbinaryが書いたplistと引き継ぎのexecのargvにある`--cmux`は新しいbinaryが受け付けて無視する（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定6、`src/main.rs`の`RetiredCmux`）。
     `up`のenvがtestのownerを名指していれば、それもsupervisorに渡し、`KeepAlive`は失敗で終わったときだけ立て直す形にして、ownerが居なくなって止まったsupervisorを立て直し続けない（[Queue service](../queue-service.md)の起動と停止）。
   - 起動: agentをbootoutしてからbootstrapし直し、既にbootstrap済みでも定義を差し替える。
     bootoutは即返り、旧プロセスが終わるまでのbootstrapは失敗するので、serviceが消えるまで待ち、消えなければSIGKILLを送る（hangしたsupervisorがlabelを`ExitTimeOut`の間占有しないため）。
   - macOSの外（launchdの無いhost）: `Launchctl`は`launchctl`を起動しない。
     `install`は「launchd mode needs macOS」のerrorで止まってplistを書かず、`down`と入れ替えのdrainはagentが載っていないとして進む。
     そのhostでは人かhostのservice managerが`dagq supervise`をforegroundで動かす（ADR-t1433-4決定4）。
   - 登録が時間内に現れなければerrorで止め、agentはそのまま残す（`launchd.log`を見る）。
     preflightに通らない環境ではKeepAliveで再起動を繰り返すので、`down`で外す。
4. **inbox**: `up`はinboxを開かない（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定3）。
   inboxは人が自分のterminalで`dagq inbox`を打って開く（下の「`dagq inbox`」）。
   `up`はplannerも開かない（plannerはruntimeが立てる。[`plan` / `planners`](plan-planners.md#plan--planners)）。
   `up`は`session_workspaces`のうちsupervisor以外のroleの行（前のbinaryが記録したもの）を消す。
   workspace自体は閉じず、人が閉じる（決定6）。
   `up`を打った環境の`DAGQ_ROLE`と`DAGQ_QUEUE`は見ない。
5. **結果**: supervisorの結果に、適用したmigration、inboxを開く案内、消した記録と登録、人が`up`の後に最初に見る未完のrun（`open_work`）を添える。

## `dagq inbox`

`dagq inbox`は、inboxを打った人のterminal（種類を問わない）の前面で開く（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定2）。
queueは`up`と同じく解決する。
inboxのproviderはClaude Codeで、Codexはinboxのcommandを持たない。

1. **inboxの中では拒む**: 打った環境が`DAGQ_ROLE=inbox`なら、`DAGQ_QUEUE`がどのqueueを指していても、何も読まず起動も記録もせずに止まる。
   inboxの中から別のinboxを開かない。
2. **preflight**: queueが`init`済みで、repositoryにmain checkoutがあること。
   Claudeとinstall済みのpluginは`up`と同じ判定で確かめ、だめなら`dagq inbox`を打ち直す案内を付けて止まる。
   言語（`dagq.toml`と利用者の`config.toml`）の間違いも止まる。
3. **記録とexec**: providerが作るinboxのcommand（inboxのsettings・plugin・inboxのprompt）を、actor executorからinboxの身元の環境付きで受け取り、cwdをmain checkoutにする。
   その前にqueueのevent `inbox_opened`に、guardrailのsettingsで開いたかとproviderを残す。
   それから`dagq`のプロセスをそのcommandでexecで置き換え、inboxはそのterminalの前面で動く。
   execが失敗したときだけerrorで戻る。
   inboxを閉じるのは人で、`DAGQ_ROLE`は起動したcommandにだけ付くので、閉じたterminalには残らない。
   - **inboxのsettings**: inboxのcommandを作るたびにqueueのディレクトリへ書き直す。
     中身はinboxのroleの`permissions.deny`だけで、hook・idle marker・サジェストの設定は入れない（inboxはidleで判定せず、人が打つ）。
   - **見せる欄**: `status`と`doctor`のinboxのguardrailは、最新の`inbox_opened`で判定する。
     前のbinaryの`up`が開いたinboxの記録も同じく判定する。
     guardrailが無ければ、引き継ぎを書き、`DAGQ_ROLE`の無いterminalでinboxを終えて`dagq inbox`で開き直すよう案内する（手順の正本はpluginの`dagq-recover`の`reference/up-down.md`の「Open the inbox again」）。
   - **限界**: settingsはClaude Codeの起動の引数なので、inboxのterminalで`claude`を打ち直したsessionと、`dagq inbox`を通さずに開いたinboxには効かず、見えない（ADR-t2159-1決定5）。
     denyはtoolの呼び出しの綴りでの照合で、絶対path・scriptの中・別のshellからの呼び出しは止めず、enforcementではない（[Security](../security.md)）。

`dagq down`はsupervisorを止める。
launchdのものも、登録済みのin-cmuxのものも、手で起動したものも同じ1コマンドで止まる。
まずPIDの生死で登録を分ける。
PIDが生きていてもheartbeatの古い登録は、`up`と共通の`pid_taken_over`で別プロセスに取られたと分かれば登録を消して結果に出す。
このPIDには通常のsignalも強制終了のSIGKILLも送らない。
プロセス一覧を読めないときとheartbeatが新しいとき、および登録より前に起動した止まったsupervisorはsignalの対象にする。
次にLaunchAgentを必ず外す（bootoutしてplistも消す。`RunAtLoad`のため残すと次のloginで復活する）。
同じqueueの別のlabelのagent（`up`の3と同じ探し方）も同じく外し、結果に出す。
そのsupervisorの登録も他の登録と同じくsignalと待ちの対象になる。
signalの対象の登録が1件もなければ`not_running`を返す（強制終了での登録行の扱いは`lifecycle::down`のdoc comment）。
あれば登録ごとにsignalを選ぶ: `mode` が`in_cmux`ならSIGINT（terminalでCtrl-Cを押したのと同じ。runtimeはSIGTERMと同じくdrainに入る。このmodeにはsignalを届けてくれるservice managerがない）、そうでなければSIGTERM——ただしbootoutでlaunchdが外したagentそれぞれのプロセスにSIGTERMを届けるので、そのPIDには送らない（runtimeは1回目のsignalでdispositionを既定に戻すので、2回目は即死になる。`lifecycle::signalled_by_bootout`）。
外したagentのどれかがloadされているのにPIDが読めないときは誰にも送らない。
既定は即返り、待つ指定はその登録が消えるかPIDが死ぬまで待ち、強制終了はSIGKILLを送って登録行を消す（leaseはやがてstaleになり、runは`doctor` / `recover`で扱う）。

queueのbroker（[Queue broker](../queue-broker.md#起動と停止)）はqueue serviceと同じ手順で起動・停止し、podmanでは動かさない（[ADR-t2114-1](../../adr/2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)）。
`up`はserviceの後、supervisorより先にbrokerを起動し（生きていればreuse、buildが違えば入れ替え）、起動できなければsupervisorを起動せずに止まる。
`down`はsupervisorの後、serviceより先にbrokerを止め、drainを待たないときは最後のsupervisorにdrainの終わりで止めさせる。

登録済みのin-cmuxのsupervisor（前のbinaryがそのmodeで登録したもの）が動いていたworkspaceを`down`は閉じず、結果に開いたまま残すと出して人に任せる（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定3・6）。
`session_workspaces`の`supervisor`行は消す。
どの経路でも`supervisors`の全登録が対象で、signalの順（in-cmuxにはSIGINT）と登録の後始末は変えない。
inboxは人のterminalで、`down`は触らない。

## CIの見張り（ADR-t1920-1）

[ADR-t1920-1](../../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](ci-watch.md)。
`[ci_watch]`があれば、`up`のpreflightは`gh`でCIを読めるかを確かめ、だめならsupervisorを起動せず、理由と対処を挙げたerrorで止まる（確かめるものと対処は`infrastructure::ci_watch::preflight`のdoc comment）。
