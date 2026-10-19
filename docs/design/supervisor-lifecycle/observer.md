---
id: design-supervisor-lifecycle-observer
type: design
title: "Observer"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1566-1
  - design-supervisor-lifecycle-prompt
  - adr-t451-1
  - design-supervisor-lifecycle
  - adr-0044
  - adr-0051
  - design-supervisor-lifecycle-commands
  - design-supervisor-lifecycle-finding-planners
  - design-domain-model
  - adr-0070
  - adr-t649-1
  - adr-t1222-1
  - adr-t1063-1
  - adr-t1204-1
  - design-supervisor-lifecycle-actor-model
---

# Observer

[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定4・18・23。
observerはnoteとdraft goalを書かず、findingを記録・更新し、見立てが人を要るfindingにだけ`blocked`のaskを上げる（下の「人が要る見立てだけをblockedにする」）。
dagqが回っているかを観察して継続的改善の材料を残すjobで、個々の詰まりは解消しない。
workspaceを持たず、`AgentProvider::headless_command`にsession idを割り当ててMCPを読まない形で起動する。
Claudeでは予約の道具（`PRINT_MODE_DENIED_TOOLS`）を拒み、promptはstdinで渡し、user・project・plugin・claude.aiのどのMCP serverも読み込まない。
providerは`[roles.observer]`の`provider`で選び、書かなければClaude、`codex`ならCodexで動く（下の「Codexで動かす」）。
実装は`src/application/observer/`（ユースケース。`compose::observe`がqueueと`ObserverSources`・`ObserverHost`を組み立てて注入する）と`src/infrastructure/observer.rs`（cursorと観察のdir、`[roles.observer]`、headlessのagentのprocess）。

Codexのobserverは、jobのdagqをクライアントモードにしてqueue service経由でfindingとfindingに紐づく`blocked`のaskを書き、Claudeのobserverも同じ経路にする（[ADR-t1222-1](../../adr/2026-10-02-t1222-1-codex-observer-writes-through-the-queue-service.md)、serviceの側のユースケースは[Queue service](../queue-service.md#ユースケース)）。
serviceはobserverのprincipal（tokenのrole `observer`とjobのactor id）をactorにしてCLIと同じ`Dialogue`で書くので、findingのeventの`by`とaskの`asked_by`は`observer`、eventのactorはjobのactor idになり、拒否の`authorization_denied`もactorのroleが`observer`で残る。
そのため下の0の「observer自身のeventを数えない」判定（`SqliteQueue::events_besides`）はCLIで書いたときと同じに成り立つ（ADR-t1222-1決定4）。
providerによらずagentの`dagq`はserviceに送り、queueのDBを開かない。
tokenはjobの起動ごとに`HostActorExecutor`が発行してfileに書き、そのpathを`DAGQ_SERVICE_CREDENTIAL_FILE`で渡し、jobのprocessが終われば失効させる（4）。
Codexのsandboxに足すのはserviceのsocketへの到達だけ（下の「Codexで動かす」）。
supervisorはserviceが居ない間observerを起動しない（ADR-t1233-4決定2）ので、agentの書き込みがserviceに届かないのは走っている間にserviceが落ちたときだけで、その件数とidは`observe_finished`に入らない（書かれていないので）。

- **`dagq observe`**: 1回のobservation。
  supervisorは行き先のlaunchなどを隠したflagで渡す（下の「Codexで動かす」）。
  0. **変化が無ければ起動しない**: `--since`も`--dry-run`も無いとき、同じmodeの直前の（skippedでない）`observe_finished`が`succeeded`で、その入力が読んだ最後のevent（agentが走っている間に他が書いたeventも読んでいないので数える）より後にobserver自身のもの以外のeventが1件も無く、今の`stats`（1の`stats --since <cursor>`と同じもの）のalertに直前のobservationの`observe_finished`の`alerts`に無いものが無ければ（[ADR-t649-1](../../adr/2026-09-28-t649-1-new-time-driven-alerts-wake-the-observer.md)）、noteなどの残りの入力を集めず、dirも`observe_started`も作らず、`outcome: skipped`の`observe_finished`だけを記録して返す。
     alertの比較は、eventを出さずに時間やその時点のsnapshotだけで閾値を超えるalertを拾うため（止まったsessionは検知のeventでskipが破れる）。
     alertの識別は`observer::alert_keys`が持ち（時間とともに伸びる値は含めない）、前回に無かった識別が1つでもあれば起動し、同じか減っただけなら起動しない。
     `alerts`を持たない`observe_finished`の後は、alertが1つでもあれば起動する。
     observer自身のものとして数えないeventは`SqliteQueue::events_besides`と、KPIの記帳のevent（`domain::kpi::BOOKKEEPING_KINDS`、ADR-0051の決定24）のdoc commentが持ち、記帳のeventだけではobserverを起こさない。
     直前が`failed` / `error`なら（何も観察できていないので）起動する。
     skippedの`observe_finished`もtimerの期日の起点になる。
  1. 入力を集める: `stats --since <cursor>`（cursorは`--since`、無ければ前回のobserveが保存した`<queue dir>/observer/cursor`、それも無ければ`stats`の既定で、`--daily`は日次の窓（`DAILY_WINDOW_SECS`）より前の最後のevent id）、未決のfinding、直近のnote、openなask、`graph`、KPIと改善のproposalの数（下の「KPIの目標割れと改善の上限」）。
     入力のJSONは`observer::observer_input`が組み立て、promptが切る一覧はpromptが残す順に並べるので、promptが省いたものは`input.json`のその一覧のoffsetから後になる。
  2. promptを作る。
     役割は「dagqが回っているかを観察し、うまくいっていないことを`finding record`でfindingに記録し（根拠はrun_eventsのidで渡し、本文に埋めない。同じ種類・対象・subjectは既存のfindingの更新になるので、持っていない根拠か読みの変化があるときだけ記録し直す）、remedyが要るfindingには`--propose`で理由を付け、もう起きていない問題は`finding resolve`し、見立てが「待てば解ける」「leave it」のものはaskにせずfindingのdetailに見立てを書いて記録・更新だけにし、見立てが人を要るものだけを`blocked`のaskでinboxに上げる（openなaskのあるfindingには上げない。下の「人が要る見立てだけをblockedにする」）。note、goal、taskは書かず、個々の詰まりは解消しない。run / task / goalの状態は変えない」。
     promptは止まったsessionの閾値の読み方を持つ（読み方の中身はADR-0043の決定6とADR-0047の決定35、ADR-0044の決定21）。
     見直しが要ればkind `threshold`・subjectに設定名のfindingを記録し、繰り返すなら`--propose`を付け、閾値そのものは変えない。
     promptはflakyなtestの読み方も持つ: statsの`failed_tests.flaky_candidates`（[stats](commands.md)）の各testを、その`integrate`のeventを根拠に`flaky_test`のfindingとして記録し、同じ根拠を持つfindingは記録し直さない。
     workerのsessionだけで落ちたtestは作業中なのでfindingにしない。
     候補から消え、windowの`failed_tests.tests`でも`integrate`で落ちていないtestの`flaky_test`のfindingは`finding resolve`してよい。
     自動更新のe2eの関門のtestも同じくflakyなtestとして読み（[ADR-t1165-1](../../adr/2026-09-30-t1165-1-e2e-gate-reruns-failed-e2e-once-and-records-quarantined-failures.md)決定4、欄は[自動更新](auto-update.md)の`e2e.tests`）、windowの関門で繰り返し落ちた（流し直しで通ったもの・印で通したものを含む。回数はpromptの文面）testを、その関門のeventを根拠に`flaky_test`のfindingとして記録する。
     安定化のtaskが要るもの（繰り返し落ちる、関門を落とした・続けて落ちている、印が付いたまま）には`--propose`を付け、runtimeのplannerが`search` / `related`で既存のtask（印のtaskもその1つ）を確かめて重複させない。
     同じ根拠のfindingは記録し直さない。
     数えるのはstatsのwindowの中だけなので、繰り返しの失敗は主に日次の観察の窓で見える。
     promptはKPIの読み方も持つ（下の「KPIの目標割れと改善の上限」）。
     根拠を読むCLIとして、自由文ではなく記録を読むコマンドを示す（ADR-0044の決定22）。
     queueのコマンドは`dagq`の形で渡す（4）。
     `--daily`は日次の窓の傾向を見る別の文面にする。
     `--dry-run`はpromptとその大きさを返し、dirを作らず何も起動・記録しない（promptが名指すobservationは`DRY-RUN`）。
     promptの渡し方・節ごとと全体の上限・省いたことの明示・byte数の記録は[Prompt](prompt.md#headlessのjobのprompt)の「headlessのjobのprompt」の節が持つ（[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)）。
     入力は下の「promptの入力の上限と選ぶ順」の上限と順で載せる。
     省いたものを読む方法は`queue_cli`の権限で打てる読むコマンド（`observe --input`と既存の読むコマンド）に限り、3の`input.json`をファイルとして読ませない（observerはファイルを読めない）。
  3. promptを組む前に`<queue dir>/observer/<started_at>/`（同じ秒に既にあれば`-1`以降の接尾辞）を作り（promptはその名前を`observe --input`の読む先として名指す）、`prompt.md`と`input.json`（入力の全体）を書き、`observe_started`をtaskの無いrun_eventsに記録する。
  4. そのdirをcwdに、envでobserverの役割とqueue serviceのsocketとobserverのtokenのfileを渡し（queueのpathは渡さない。tokenはjobの終わりで失効する）、PATHの先頭に`dagq`のdirを置いてagentを起動し（promptは`dagq --db <db>`ではなく`dagq`を名指し、agentの`dagq`はクライアントモードでserviceに送る。[Queue service](../queue-service.md#クライアントモード)）、stdoutを`output.out`、stderrを`output.err`に分けて書く（`Streams::Files`）。
     `--timeout`を過ぎたらkillする。
     killの前にagentの子孫（`ProcessControl::descendants`。agentのBashとそれが起動したもの）を読み、agentの後にそれらもSIGKILLするので、agentの子プロセスは残らない。
     Codexのjobも同じ`run_agent`で止め、Codexがコマンドごとに別のprocess groupで走らせたものも、killの前に列挙した子孫としてpidで止まる。
  5. 終了後、`observe_started`より後にobserverが記録・更新したfindingと、observerが閉じたfinding（`resolved`か`dismissed`にしたもの。再記録による開き直しと、人やplannerの`resolve` / `dismiss`は数えない）とobserverのaskを集め、`observe_finished`を記録して返す。
     `outcome`は`succeeded`、`failed`（非0終了）、`error`（起動できない・timeout）のどれかで、`consecutive_failures`はmodeを問わず続く`error` / `failed`の回数。
     `succeeded`のhourlyのときだけ`cursor`を`<queue dir>/observer/cursor`に保存する（一時ファイルからrename）。
     失敗したobservationのwindowは次のobservationが読み直す。
     dailyはcursorを動かさない。
     保存する`cursor`は`stats`の`next_cursor`（runのpageの続き）のままなので、終わったrunを飛ばさない。
     `stats`の期間集計はpageに依らず窓の終わりまで数える（`domain::stats`のmodule docの期間の窓）ので、observationの間隔のうちにpageを超えるrunが終わったときだけ、次のobservationの期間集計が前のobservationと重なる。
     observationごとの期間集計を足し合わせず、1回の値をその窓の値として読む。
     `consecutive_failures`が`OBSERVER_FAILURES_TOLD`に等しい`observe_finished`はinbox宛てのattention（`check the failed observer`）になり、`--role inbox`のwatchを起こす。
     ほかの回数と成功に戻ったものはattentionにしないので、1つの連続に1回だけ届く（毎時のobserverが止まり続けてもinboxを毎時起こさない。ADR-t1418-1）。
     askにせず、claimと着地を止めず、`status`にも残らない（`domain::event_attention`）。
- **`dagq observe --history`**: 過去のobservationを新しい順に返す（`observe_finished`ごとに1件で、各件の中身は`observer::history`）。
  読むだけなのでqueueを読み取り専用で開き、observerのenvからも打てる。
  終了した観測の出力をruntimeが読み直して分類することはない。
  失敗時の分類は、Claudeのjobは`AgentSignals::job_failure`に両streamを改行でつないで渡し（壁ならqueueのhold askに入る）、Codexのjobは役割がproviderを書いたときだけ`AgentProvider::job_failure(stdout, stderr)`で読む（下の「Codexで動かす」の`provider_unusable`）。
  成功時は分類しない。
- **`dagq observe --input OBSERVATION`**: observation（`observe --history`の`dir`の名前）の`input.json`、promptが省いたものを含む入力の全体を読む（読み方は`infrastructure::observer::read_input`）。
  読むだけで、queue serviceの読み取りのユースケース`observe_input`（[Queue service](../queue-service.md#ユースケース)、`queue.read`）なので、observerのenv（クライアントモード）からも打てる。
  observerのClaudeは`Bash(dagq:*)`だけでファイルを読めないので、promptはファイルのpathではなくこのコマンドを読む方法として書く（Codexのobserverはread-onlyのsandboxでファイルを読めるが、promptはproviderに依らず同じ）。
- **権限**: observerのenvからのCLIは許可一覧で判定する（`main.rs`の`check_access`が`StaticPolicy`に問い、計画系と対話・記録系（ask・answer・note・mark・finding）のコマンドはapplicationの層が判定して拒否を`authorization_denied`に記録する。policyの表は[Authorization](../authorization.md)、一覧は[domain-model](../domain-model.md#current-operations)）。
  読み取り（`observe --history`と`observe --input`を含む）、`finding record` / `finding resolve`、`ask --kind blocked`だけが通り、ほかの状態を変えるコマンドは拒否される（[Authorization](../authorization.md)の表）。
  jobのagentはクライアントモードなので、実際にはserviceのユースケースの無いコマンドはクライアントの側で`no_use_case`、serviceに届いて認可されないもの（note・runに紐づくaskなど）はserviceが`authorization_denied`で拒んで記録する（[Queue service](../queue-service.md#クライアントモード)）。
  Codexのobserverには、Claudeのdagqだけを許す道具の代わりのhookやrulesを作らない: read-onlyのsandboxが書き込みと他のprocessへのsignalを拒み、queueへの書き込みはserviceの認可だけが通す。
  Claudeのobserverにhookと拒むコマンドの一覧の設定は無いので、その代わりも作らない（ADR-t1222-1決定5）。
- **timer**: `supervise`の間隔の設定（hourlyとdaily）。
  ループの各passで、走っているobserverが無ければ、dailyが有効で最後のdailyの`observe_started` / `observe_finished`から日次の窓が経っていればdailyを、そうでなく最後のhourlyから間隔が経っていればhourlyを、`dagq observe`の子プロセスで起動する（cwdはcheckout、`DAGQ_ROLE`は外す。Codexの実行ファイルはworkerと同じく`supervise --codex`で解決したもの。行き先と`--no-claude`は下の「Codexで動かす」）。
  一度も記録の無いmodeは期日が来ている。
  期日はqueueのrun_eventsで判定するので、別のsupervisorや手の`observe`も数える。
  加えて同じプロセスが同じmodeを起動してから間隔が経つまでは再起動しない（記録を書く前に落ちたobserverを毎passで起動しないため）。
  同時に走るobserverは1つで、run slotを使わない。
  子プロセスの終了はlogに1行残し、記録は`observe_finished`が持つ。
  supervisorのループがerrorで終わったときは（正常な停止はobserverの終了を待つ）、走っている`dagq observe`の子孫を読んでから`dagq observe`とその子孫（agentとそのBash）をSIGKILLし、logに1行残す。
  この`observe`は`observe_finished`を書かない。
  execの引き継ぎ（`prepare_handoff`）でも同じく子孫ごとkillし、次のプロセスが期日に起動し直す。
  どのsupervisorでも`supervise`の既定値で同じに動く（`up`はこの設定を渡さない）。
  起動するのはqueue serviceが居るときだけ。
- **出力の扱い**: findingは人とplannerが`findings`で影響の大きい順に読み、手当てしないと決めたものを`finding dismiss`する。
  proposalを求める印の付いたfindingには、supervisorがruntimeのplannerを立ててproposalを作らせる（[Finding planners (supervisor)](finding-planners.md)）。
  findingに紐づけた`blocked`のaskには、runtimeが`propose`と`dismiss`のoptionを足し、人のその答えをfindingに適用する。
  `blocked`のaskは`status --role inbox`に`ask_opened`として出る。

- **生死と通知**: `stats`の`workspace_mismatch`はrunのbackgroundのwrapperの生死で決まり（[Stats](commands.md)）、agentがloginの切れや利用上限で止まったときのholdのask（`application::ask::hold`）はinboxの`watch`が`ask_opened`で人に知らせる（[人への通知](ask-push.md)）。

## Codexで動かす

[ADR-t1222-1](../../adr/2026-10-02-t1222-1-codex-observer-writes-through-the-queue-service.md)と[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)の決定1・4・5。
`dagq.toml`の`[roles.observer]`に`provider = "codex"`を書くと、observerのjobはCodexの`codex exec`で動く（[Actor model](actor-model.md#provider)の`CODEX_ROLES`）。
promptと打たせるコマンドと出力の形は変えず、Claudeのplugin・skill・hookに頼らない。
設定は観測ごとに読み直すので、着地してmain checkoutに反映されればsupervisorの再起動は要らない。

- **sandbox**: 権限の意図`ACCESS`（`queue_cli`）のまま、Codexの実装が読み取りだけのsandboxに訳す。
  queue serviceが居れば、`:read-only`を継いでserviceのsocketへの到達だけを足したjobのpermission profile（`codex::job_service_config`）で起動し、書けるroot・network・sandboxを外すflagは渡さない（[Agent provider lifecycle](../provider-lifecycle.md#codexのheadless-job)）。
  queue dirとDBのファイルにも書けないので、queueへの書き込みはjobの`dagq`がクライアントモードでserviceに送るものだけで、serviceがobserverのprincipalで認可する（ADR-t1222-1決定1・2）。
- **行き先**（`Supervisor::job_start_route`に`ModelRole::Observer`。スループットの見直しと共有）: providerを書かない役割はClaudeで、queueのholdのあいだと`--no-claude`では起動しない（[ADR-t1204-1](../../adr/2026-09-30-t1204-1-explicit-no-claude-operation.md)の決定2）。
  providerを書いた役割は`job_route`で、そのproviderが使えればそこで、使えなければもう一方のproviderで（launchに切り替えの元と理由）起動し、どちらも使えなければ待つ。
  `[provider_fallback] jobs = false`（[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）のときは、使えない理由ではもう一方で起動せず、控えのあいだ待つ（[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)の「切り替えを止める設定」）。
  `--no-claude`ではClaudeは使えないので、Codexを設定したobserverはCodexで起動し、Codexも使えなければ子プロセスに`--unavailable`で理由を渡す。
  子プロセスは0の判定でskipしなければ、agentを起動せずdirも`observe_started`も作らずに、理由を持つ`outcome: error`の`observe_finished`を記録する。
- **子プロセス**: supervisorは行き先のlaunchを`--launch`で渡し、コマンドはその`provider`のagent（`ClaudeCode`か`Codex`）で起動する。
  `--launch`が無い（人が手で打つ）ときは束縛されたcheckoutの`[roles.observer]`（`compose::observer_launch`）。
  Codexの実行ファイルを解決できない（supervisorが見つけた後に消えた）ときもコマンドは失敗せず、与えられたpathのまま起動を試み、起動の失敗を`provider_unusable`（`executable_missing`、`application::job_start_failure`）として`--switchable`のとき終わりに記録する。
  Claudeの実行ファイルは解決できなければ失敗し（`--switchable`と`--no-provider-fallback`のときはCodexと同じく与えられたpathのまま起動を試み、`executable_missing`を記録する）、起動しない側のproviderの実行ファイルは解決しない
- **記録**: Codexのthreadは始まりの記録に無く、終わりが記録する（threadはCodexが名付けるので）。
  actorはobservationごとのid。
  `observe_finished`と区間の`session_closed`（区間は`dir`で閉じる）にCodexのsessionが入り（`JobSession::record`）、`stats`と`kpi`のjobの集計でproviderとmodelの層に分けて読める（[stats](commands.md)）
- **Codexが使えなかったとき**: `--switchable`（役割がproviderを書いた）でCodexのjobが成功しなかったとき、providerの`job_failure`（`output.out` / `output.err`。agentが起動しなかったときは起動のerror）が使えないこと（認証・利用上限・起動の失敗・実行ファイルが無い）を示せば、`observe_finished`に`provider_unusable`を書く。
  Codexの失敗はClaudeのqueueのhold askに入れない。
  supervisorは自分の子のその終わり（起動時のevent idより後の同じmodeの`observe_finished`）を見てCodexを控え（`hold_provider`。利用上限はCodexの出力から再開の時刻を読む）、そのmodeをこのprocessの起動の記録から外して期日にする（`observer_again`）ので、次のpassでもう一方のproviderで起動し、`--no-claude`では上の`--unavailable`で理由を記録する。
  控えを書けなかったときは間隔を待つ（同じCodexをすぐ起動し直さないため）。
  `[provider_fallback] jobs = false`のとき（supervisorは`--switchable`に加えて`--no-provider-fallback`を渡す）: Codexを書いた役割は上と同じくCodexを控えてmodeを期日にするが、次のpassはClaudeで起動せず、Codexの控えが解けた後にCodexで起動し直す。
  Claudeを書いた役割は、Claudeが使えずに終わったとき（起動の失敗は起動のerrorから、認証・利用上限は`signals.job_failure`の壁の読みから。`read_failure`）に`provider_unusable`（`claude`）を記録し（壁では子プロセスが`hold_wall`で控えのaskを開く）、supervisorは認証・利用上限なら同じ控えのask（`HoldJob::Observer`。既に入っていれば重ねない）、起動の失敗なら`ProviderHold`（控えのaskが既にClaudeを控えていれば重ねない）でClaudeを控え（`hold_timer_jobs_unusable`。Codexは控えない）、`observer_again`で期日にし、控えのあいだ待って、控えが解けた後（askの`done`か`retry_at`）のpassでClaudeで観測し直す。
  onのClaudeを書いた役割は`provider_unusable`を記録しない
- **時間の上限**: 4のとおりClaudeと同じ`run_agent`でjobと子孫をpidで止める。
  execの引き継ぎとsupervisorのerrorでは、timerの節のとおり`dagq observe`とその子孫（Codexとその子孫）をkillする

## promptの入力の上限と選ぶ順

[ADR-t1566-1](../../adr/2026-10-03-t1566-1-headless-job-prompts-carry-decision-material-within-limits.md)の決定2〜6。
`observer::observer_prompt`が指示の後に入力を節ごとに載せる。
実装は`src/application/observer/input.rs`の`fit`（`observe --input`のファイルの読み取りは`infrastructure::observer::read_input`）、上限の値と理由はその定数のdoc comment。

- **全体の上限**: `PROMPT_LIMIT`（言語の指示を含み、そのために`LANGUAGE_RESERVE`を取っておく）。
  promptをstdinで渡すjobも同じ上限を持つ（上限はagentの文脈のため）。
- **必須の節**（切らない）: 指示（役割・windowとcursor・書けるものと禁止・読み方・読むコマンド）と、`observe --input`の読む先のobservationの名前。
  指示と節の見出し・省いた注記だけで上限を超えるときも指示と見出しは切らず、全ての資料を省いてそれぞれの読む方法を書く（このときだけpromptは上限を超える）。
- **必須の資料**（自分の上限を持たず、全体の上限だけで切られる。他の節より先に場所を取る）: 順に`kpi.breaches`（findingにする目標割れ）、`open_asks`（同じfindingにaskを重ねない）、`stats.alerts`と`stats.running_alerts`（observationを起こしたalert）。
  これだけで全体を超えるときも黙って切らず、他の節と同じく省いた件数と読む方法を書く。
- **省いてよい節**（必須の資料の残りを順に分け合い、それぞれの上限の中。上限と`stats`のkeyの順は`observer::input`の`Spec`と`STATS_ORDER`）:

| 節 | 形 | 選ぶ順 | 省いたものの今の読み方 |
| --- | --- | --- | --- |
| `stats` | key（`alerts`・`running_alerts`を除く） | `STATS_ORDER`、残りはkeyの名前順 | `stats --since <cursor>` |
| `kpi` | key（`breaches`を除く） | 決めたkeyの後に名前順。`trend`と`forecast`は要約する（`forecast.*`の値は載せない。偏りは`kpi.breaches`だけで読む） | `kpi` |
| `findings` | 一覧 | `impact`のhigh・normal・low、`occurrences`の多い順、新しい（idの大きい）順 | `findings`・`findings ID --full` |
| `improvements` | key | 名前順 | `findings` |
| `notes` | 一覧 | `notes`の順（直近） | `notes` |
| `graph.critical` | 一覧 | `graph`の順 | `graph` |
| `graph.candidates` | 一覧 | `graph`の順 | `candidates` |

- **選び方**: 節を上の順（必須の資料、省いてよい節）に、項目を節の順に、節の上限と全体の残りに入る間だけ取る。
  一覧は入らない最初の項目で止め（省いたものはoffsetから後の続き）、keyの節は入らないkeyを飛ばして次を試す。
  見積もりは件数の桁と節の注記を含まないので、組んだ後で全体が上限を超えれば、後ろの節の最後の項目から落とす。
  LLMには選ばせない。
- **項目の縮め方**: 一覧の1件はcompactなJSONの1行。
  文字列は`TEXT_CHARS`で切って残りの文字数を書き、項目の中の一覧（根拠のid・印・期間の値）は最後の`NESTED_ITEMS`件（queueの一覧は古い順なので新しいもの）にして残りの件数を書く。
  keyの節の値は縮めず、入れるか省くか（`kpi`の`trend`・`forecast`は上の要約）。
- **省いたことの明示**: 節ごとの見出しに残した数と全体の数を書き、省いたら`observe --input`でその節の続きを読む方法と表の今の読み方を（keyの節は省いたkeyの名前も）書き、要約したkeyと縮めた項目にもそれを読む`observe --input`を書く。
  `observe --input`はobservationの時点の入力をそのまま返し、表の今の読み方は今の状態を返す。
- **記録**: `observe_started`がpromptの大きさと節ごとの内訳を残し、`observe --history`と`--dry-run`も同じものを返す。
  起動に失敗したobservation（引数の大きさなど）も`observe_started`に残る。

## KPIの目標割れと改善の上限

[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定24〜26（決定26はADR-t1971-1がamends）。
observerはKPIの目標割れの継続を種類`kpi`のfindingにし、そこから作る改善のproposalは同時の数に上限がある。

- **入力の`kpi`**（`OneShot::observer_kpi`、純粋関数と載せるものは`domain::kpi::observe`）: [`kpi`](commands.md)を直近の日と週の期間で`dagq kpi`と同じ設定とhostの負荷の記録で計算し（hostの負荷のKPIの目標も判定）、目標ごとの判定、目標割れごとのfindingの種類・subject・根拠のevent・印、期間ごとの傾向と見込みの答え合わせを載せる。
  数字は`kpi`の出力をそのまま引き、observerは作らない。
  読めなければ`{"error": ...}`にして他の入力で観察を続ける。
  目標割れの根拠は同じ期間・KPI・層の閉じていない`kpi_breach_started`のevent ID（`SqliteQueue::kpi_breach_events_open`）。
- **入力の`improvements`**（`application::kpi::improvements`）: 動いている改善の数と上限、上限に達しているときにplannerを待つfinding。
  `dagq findings`も同じものを`improvements`として返す。
- **promptの読み方**（`observer::observer_prompt`）: `breaches`の各項を、その種類・subject・根拠のeventでqueueのfindingに記録し、目標割れの値は`kpi`の出力をそのまま写す。
  根拠が無い（まだ記録されていない）目標割れは次のobservationに回す。
  日と週の同じKPIと層、続いている目標割れは`subject`が同じなので1件のfindingにまとまり（ADR-0044の決定18）、新しいeventがあるときだけ記録し直す（決定21）。
  `missed`と`not_judged`はfindingにしない。
  `kpi`か`forecast`のfindingの目標が`ok`に戻ったら、戻った期間を理由に`finding resolve`する。
  影響と続いた期間から改善が要ると読めば`--propose`を付け、目標割れを`blocked`のaskにはしない。
  `trend`は印と並べて読み、入力に無い数字を計算しない。
- **改善の上限**（[Finding planners](finding-planners.md)の3と11）: runtimeのplannerがfindingに紐づけて出したproposalのうち終わっていないもの（人が開いたplannerのproposalは数えない）と、`open`のfindingのために立ったruntimeのplannerの数が、`dagq.toml`の`[kpi]`の`max_improvement_proposals`（host.tomlでは変えない）に達しているあいだ、supervisorは印の付いたfindingに新しいplannerを立てない。
  findingは`open`のまま待ち、1つ終われば印の古い順に立つ。
- **優先度**: 改善のproposalのtaskは個別の優先度を持たずgoalから継ぎ、`normal`以下はgoalと所属で守る（[Finding planners](finding-planners.md)の6）。

## 完了見込みの誤差

[ADR-0070](../../adr/0070-forecast-snapshots-and-scoring.md)の決定5。

- **入力**: 入力の`kpi.forecast`（上）に、日と週の期間ごとの答え合わせの指標と件数が層ごとに載る。
- **偏りの判定**: 「偏りが続く」は`forecast.*`の目標（`dagq.toml`の`[kpi.targets]`。planner が書く）の目標割れ（`breach`）だけで、別の経路は作らない。
  observerは`kpi.forecast`の数字から自分で偏りを判定しない。
- **finding**: `forecast.*`のKPIの目標割れは`kpi.breaches`に`finding_kind: forecast`、`subject`は接頭辞`forecast.`を除いた`<指標>/<層>`で載り（`domain::kpi::observe`）、observerは種類`forecast`・対象`queue`のfindingを記録か更新する。
  根拠・日と週のまとめ・解消・`--propose`はKPIの目標割れと同じで、改善のproposalの上限（ADR-0051の決定25）と優先度（ADR-t1971-1）も同じく効く。
  `blocked`のaskにはせず、人には上げない。
- 見込みのsnapshotのeventは記帳のeventとしてobserverを起こさない。
  `dagq forecast`は読み取りのコマンドで、promptの読むコマンドに並ぶ。

## 人が要る見立てだけをblockedにする<a id="人が要る見立てだけをblockedにする未実装"></a>

[ADR-t451-1](../../adr/2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)の決定2（ADR-0047決定4・23をamends）。
observerは見立てが人を要るfindingにだけ`blocked`のaskを上げ、ほかの見立てはfindingに残す。

- **prompt**（`observer::observer_prompt`）: 問題ごとに見立て（次に何が起きるべきか、それを人がする必要があるか）を出させる。
  見立てが「待てば解ける」「leave it」のものは`blocked`のaskにせず、findingの`--detail`に見立てを書いて記録・更新だけにする（人は`dagq findings`で見る）。
  `blocked`のaskにするのは、見立てが人の判断（受け入れ条件・範囲・ADR・goalの決定か、成果を捨てるか）か、runtimeと復旧jobが行えない人の手の操作を要るときだけ（理由の分け方は[ask](ask.md)の「人が要る理由」）で、認証と費用はqueueの控えのaskが持つのでblockedにしない。
  askには人が選べる次の手をoptionに、見立てを推奨のoption（optionのどれかか、runtimeが足す`propose` / `dismiss`）と確信度で載せる（[ask](ask.md#aiの推奨と確信度未実装)）。
  終わりの1行には、askにせずfindingだけにした件数も書かせる。
- **CLI**: `ask --kind blocked`は`--recommend`が無ければ（空白だけも）理由つきで拒む（`NewAsk::check_asker`を`Dialogue::ask`が呼ぶので、CLIとqueue serviceの`ask`の両方。[ask](ask.md#aiの推奨と確信度未実装)）。
  推奨がleave it / waitに当たるかはCLIでは判定しない（promptの規則）。
- **集計**: `observe_finished`の`findings_without_ask`は、そのobservationが記録か更新し閉じなかったfindingのうち、そのobservationが開いた`blocked`のaskも、終わった時点でopenな`blocked`のaskも無いものの件数（`SqliteQueue::written_by`の`without_ask`）。
  skippedの`observe_finished`は0と空。
  [Stats](commands.md)の`recommendations.observer`がwindowの合計を`blocked`のaskの件数と並べ、`recommendations.decided_without_ask`の`blocked`（AIが決めてaskにしなかった判断）に足す。
- findingごとにopenな`blocked`のaskは1件のまま（決定23）。
  askの無いfindingは普通の状態で、人は`dagq findings`で見る。
