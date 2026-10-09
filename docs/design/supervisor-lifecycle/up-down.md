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
  - design-supervisor-lifecycle-landing-branch
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

`dagq up [--parallel N] [--max-waiting N] [--runtime-planners N] [--max-load N] [--no-wait] [--handoff-timeout SECS] [--plugin-dir PATH] [--repo PATH] [--cmux EXE] [--claude EXE] [--codex EXE]`はqueueのruntimeをcold startする1コマンドで、application層の`src/application/lifecycle.rs`の`up`が行う（`down`も同じファイル。[ADR-0013](../../adr/0013-layered-architecture-and-type-function-style.md)）。queueは`QueueOpener`が開くhost運用のport（`LifecycleQueue`）、repositoryのrootとcommon dirは注入された`inspect_repository`、Claude Codeのpreflightは`AgentProvider`、folder trustの読み取りは注入された`trusts_repository`、pathの正規化は`RunFiles`、時刻は`Clock`から得て、これらは`lifecycle::Ports`にまとめて`compose::up` / `compose::down`が組み立てる。冪等で、続けて2回叩けば2回目は`reused`になる。`up` / `down`はcmuxを呼ばない（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定1）。外部（launchctl、PIDの生存とsignal）は`LaunchAgent`、`ProcessControl`のtrait越しに呼び、`tests/it/lifecycle_*.rs`（共通のfakeは`tests/common/lifecycle.rs`）はfakeで判定を、`tests/e2e.rs`は実launchdで`up → status → down --wait`を確認する。
このlaunchdのe2eは使い捨てのHOMEとLaunchAgentのlabelで動き、本番のsupervisorのagentに触れず、追加の環境変数なしで`--ignored`の実行で本文を流す。
launchdの無いhostでは`up`がlaunchctlを起動する前に「launchd mode needs macOS」で止まり（下の「macOSの外」）、testは落ちる（skipもせず、黙ってpassもしない）。
cmuxが答えない関門はこのe2eを流さず、流さなかったことを残す（[Validation](validation.md#runtimeが流すe2e)）。
`up --in-cmux`は何にも触る前に拒み、in-cmux modeの廃止（supervisorはcmuxを呼ばずlaunchdで常駐する）と次の手順（`down --wait`の後に`--in-cmux`を外した`up`。`--auto-update`などいつもの引数は付けたまま）を文面に書く（[ADR-t1433-4](../../adr/2026-10-03-t1433-4-supervisor-resides-without-cmux.md)）。

1. **preflight**: queueが`init`済み（DBが存在する。`--db`がなければcwdのrepositoryから解決）、repositoryのroot、Claude（`--version`）、`--plugin-dir`が無ければinbox（`dagq inbox`）とruntimeのplannerが読むinstall済みの`claude-dagq`が有効なこと（Claudeと合わせて`lifecycle::require_agent`で、`dagq inbox`も同じ判定を行う。`--claude`で`claude plugin list --json`をrepository rootで実行して確かめ、未install・無効・コマンドの失敗・出力を読めないときは公式の手順のinstallのコマンド（`claude plugin marketplace add hisamekms/dagq`と`claude plugin install claude-dagq@dagq`）を英語で案内し、末尾に`; the supervisor was not started`を付けたerrorでsessionもsupervisorも開かずに止まる。`--plugin-dir`を付けたときと、runtimeがsupervisorを起動し直すために`DAGQ_UP_RESTART=1`の環境で打つ`up`（`install --allow-breaking`のdrainの後と自動更新の後）は確かめない。`dagq plan`は何も開かずに案内を付けて拒むので（[ADR-t1394-1](../../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)）、この確かめを行わない。[ADR-t617-2](../../adr/2026-09-27-t617-2-installed-plugin-by-default-plugin-dir-for-development.md)の決定4、確かめ方と文言の全体は[plugin integration](../plugin-integration.md#installしたpluginの確認)）、Claude Codeがrepository rootを信頼済みであること（`$CLAUDE_CONFIG_DIR/.claude.json`、未設定なら`~/.claude.json`の`projects[<root>].hasTrustDialogAccepted`が`true`。run worktreeの信頼はrepository rootから決まるので、未信頼のまま流すと全runがtrust dialogで止まる。未信頼・configが無い・HOMEが無いときは、rootで`claude`を一度起動して承認する案内のerrorで、何も起動せずに止まる。configを書き換えて信頼を代行することはしない。[provider-lifecycle](../provider-lifecycle.md#trust-prompt)）、main checkoutの`dagq.toml`の`[run.env]`が名指すプログラム（`RUSTC_WRAPPER`など）が`up`のPATHで解決できること（見つからなければ変数名・値・PATHを挙げたerrorでsupervisorを起動せずに止まる。`dagq.toml`があれば出力に`run_env`が付く。[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定9、[Run environment](run-environment.md#runenvが名指すプログラムの検査)）。`--plugin-dir`は絶対pathに正規化する。
   - trustを確かめるrepository rootと、`[run.env]`・言語・着地先のbranchを読む`dagq.toml`は、repositoryのmain checkout（main worktree。[Run environment](run-environment.md#main-checkoutの決め方)）のもの。main checkoutが無いrepository（bare、またはlinked worktreeから打った`--separate-git-dir`）では、理由と案内をつけて`...; the supervisor was not started`で止まる。
   - 着地先のbranchの解決（`dagq.toml`の`[repository]`か推定）もpreflightで行い、解決できなければsupervisorを起動せず`dagq.toml`での指定を案内して止まり（`... ; the supervisor was not started`）、明示した`remote`が無い（`push = false`でない）ときも同じく止まる。通れば出力に`repository`（`branch`・`branch_source`・`remote`・`remote_source`・`remote_exists`・`push`）が付く（[Landing branch](landing-branch.md#upのpreflightとdoctor)、ADR-t615-1）。
   - `--auto-update`を付けた`up`は、着地先のbranchの解決の後で、queueのrepository（main checkout）がdagqのソースかを確かめ、ソースでなければsupervisorを起動も引き継ぎもせずにerrorで止まる（`lifecycle::auto_update_refused`の文面に`; the supervisor was not started`が付く。自動更新はrepositoryのソースからdagqをbuildするため。[ADR-t614-1](../../adr/2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)）。`--auto-update`を付けなければ判定しない。文面と判定は[Auto-update](auto-update.md#dagqのソースのrepositoryだけ)と[Source repository](source-repository.md)にある。
2. **stale登録の削除**: `supervisors`表のうちPIDが死んでいる行を`prune_supervisor`で消し、消したtokenとpidを結果の`pruned_supervisors`に出す（`binary_version`がnullの行も同じ）。PIDが生きていてもheartbeatが30秒より古い行は、そのPIDを登録したプロセスでないと分かれば同じく消し、`pruned_supervisors`の要素に`reason: "pid_reused"`を付ける: `ProcessControl::list`（`ps -U <uid>`）のこのuserのプロセスに無い（別のuserのプロセスがPIDを使っている。supervisorは必ずこのuserのプロセス）か、`elapsed_secs`から求めた開始時刻が登録の`started_at`より5秒（`PID_START_SLACK_SECS`。`ps`の経過時間は秒単位）を超えて後（登録より後に始まったプロセスは登録したプロセスではありえない。handoffのexecは開始時刻を変えない）。一覧は1回の`up`で最初に要ったときに1回だけ読み、読めなければどの行もこの理由では消さない。`run_leases`は触らない（そのrunの復旧は`doctor` / `recover`の仕事）。登録より前に始まったプロセスがPIDにいてheartbeatが30秒より古い登録（hang）は消さず、reuseもしない。
3. **supervisor**: 生きていてheartbeatが新しい登録があり、その`binary_version`が全部`up`自身のbuild識別子（`dagq::VERSION`。下記[Build identifier](build-identifier.md#build-identifier)）と文字列として同じなら`{"outcome":"reused","mode":…,"version":…,"pid":…}`で、plistにもlaunchctlにもcmuxにも触らない（`mode`はその登録に記録されているものをそのまま返し、手で起動したsupervisorはnullのまま）。1つでもversionが違えば**入れ替える**（下記）。liveな登録が無ければlaunchd modeで起動する。
   - **launchd mode**: LaunchAgentを書いて起動し、登録が現れるまで（30秒）待って`{"outcome":"started","mode":"launchd","pid":…,"workspace_id":null,"plist":…}`。
   - **登録済みのin-cmuxのsupervisor**（ADR-t1433-4決定3）: `up`は新しくin-cmux modeで起動せず、登録に`in_cmux`も`workspace_id`も、`session_workspaces`の`supervisor`行も書かない。
     読むのは前のbinaryが残した登録（`mode`が`in_cmux`で`workspace_id`を持ち、argvに`--cmux`を含む）だけで、引き継ぎ（下記）ではそのworkspaceに居続けてcmuxを呼ばず、drainと`down`ではSIGINTを送る。そのworkspaceはdagqが閉じず人が閉じ、`session_workspaces`の`supervisor`行はcmuxを呼ばずに消す（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定3・6）。
     launchdに移すのは人かinboxの一度の`down --wait`と、`--in-cmux`を外した`up`。
     起動するsupervisorに渡すhiddenの`--mode`は常に`launchd`で、supervisorの起動の印`supervisor_started`の`mode`になる（[変更の印](marks.md)）。
   - **待つ相手**: `wait_for_registration`は`up`が起動したsupervisorの登録だけを受け取る。2で見た時点で既に表にあったtoken（PIDが生きていてheartbeatの古い「生きているが黙っている」登録を含む。`up`はこれをpruneもreuseもしない）は除外する。除外しないと、待っている間にその古いsupervisorがheartbeatを再開したときにそれを自分が起動したものと取り違え、`launchd`のmodeを別プロセスの行に書き、報告するpidとtokenと`--auto-update`を書く先も取り違える（`supervisors`は`started_at`順なので古い行が先に出る）。
   - **modeの記録**: 起動したsupervisorが登録に現れた直後に、`up`が`supervisors`の行へ`mode`（`launchd`）を書く（`set_supervisor_mode`。schema v9）。書くのは`up`だけで、`supervise`自身は書かない。modeはプロセスの性質なので登録行と寿命を共にし、supervisorがgracefulに終われば行ごと消える（queue dirのsidecar fileにしなかった理由は[persistence](../persistence.md)のRuntime ownership）。`status` / `doctor`は`supervisors[].mode`と`workspace_id`として出し、`down`はこれを見てSIGTERM（launchd mode / 手起動）とSIGINT（登録済みのin-cmux）を使い分ける。
   - **並列数と待ちの上限**（`--parallel N`と`--max-waiting N`、[ADR-0062](../../adr/0062-runs-waiting-for-a-person-leave-the-slot.md)の決定7）: 明示されたときだけ`supervise_arguments`に`--parallel N` / `--max-waiting N`を足す。明示が無ければ既定の4を焼き込まず、supervisorがmain checkoutの`dagq.toml`の`[supervisor]`を読んで決め、無ければ4にする（[Run environment](run-environment.md)の`[supervisor]`）。flagで起動したsupervisorは`dagq.toml`の変更に従わないので、`[supervisor]`で決めるなら`up`にflagを付けない（付けて起動したものは`down --wait`の後にflagなしの`up`で起動し直す）。supervisorが値と出どころを登録の`parallel` / `max_waiting`に書く（[人の答えを待つrun](waiting.md)）。
   - **runtimeのplannerの上限**（`--runtime-planners N`、ADR-0041の決定12）: `--parallel`と同じく明示されたときだけ`supervise_arguments`に`--runtime-planners N`を足す。明示が無ければ既定の1を焼き込まず、supervisorが`[supervisor]`の`runtime_planners`を読んで決め、無ければ1にする。値と出どころは登録の`runtime_planners`に書かれ、`status`と`doctor`に出る。
   - **claimを控えるload**（`--max-load N`、0以下で控えを無効）: 与えたとき（0を含む）だけ`supervise_arguments`に`--max-load N`を足す。与えなければ引数に足さず、起動したsupervisorが自分のhostの論理CPU数の2倍を既定にする。0未満は負の数がflagに読まれないよう`0`として渡す（[claimを控える](claim-hold.md)）。
   - **自動更新**（`--auto-update`、[ADR-0045](../../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定17）: 起動するsupervisorは`supervise --auto-update`で起動し、reuseで残すliveな登録と、引き継いだsupervisorの登録には`supervisors.auto_update`を書く。引き継いだsupervisorには結果の`replaced`の`token`、つまり今のtoken（pidが新しいtokenで登録し直したならそのtoken）に書き、引き継ぎに失敗して前のbinaryのまま動いているsupervisorにも、その登録が残っていれば前のtokenに書く（残す登録の設定は`up`が指定したものにそろえる。前のbinaryが自動更新を持っていれば、それが次の着地で自分でbuildして`install`し、もう一度引き継ぎを試みる）。消えた登録には書かない。付けない`up`は同じ登録の`auto_update`を消す。結果の`supervisor.auto_update`に出る（[Auto-update](auto-update.md#auto-update)）。queueのrepositoryがdagqのソースでなければ、ここまで来ずに手順1のpreflightで止まる（[ADR-t614-1](../../adr/2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)、[Source repository](source-repository.md)）。
   - **buildの違うsupervisorの入れ替え**（[ADR-0045](../../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定10・12・15・16。ADR-0014を置き換えた）: 登録の`binary_version`は`supervise`プロセス自身が`register_supervisor`で書いた自分のbuild識別子で、列（schema v10）より古いbinaryの行はnull。`up`はliveな登録のどれか1つでも自分のbuild識別子と違えば（nullを含む。package versionが同じでもcommitやdirtyが違えば違う）入れ替える。liveな登録が全部引き継ぎを受けられる（`handoff_accepted`）なら**引き継ぎ**、そうでなければ**drain**で入れ替える。launchd modeの登録は、LaunchAgentのplistの`ProgramArguments`の先頭が`up`自身の絶対pathのときだけ引き継ぐ（execはplistを変えないので、pathの違うagentはlaunchdの次の再起動で前のbinaryに戻る。そのときはdrainし、`up`がplistを書き直す）。
     - **migrate**: `up`はまずqueueの未適用のmigrationを数え（`SqliteQueue::migrate_compatible`）、全部が互換なら`SqliteQueue::migrate`で適用して結果の`migrated`に載せる（無ければnull）。非互換が1つでもあれば、何も起動もsignalもせず、`down --wait` → `dagq migrate` → `up`、または`dagq install --allow-breaking`を案内するerrorで止まる。互換のmigrationの後も、古いbinaryのsupervisorとrunのwrapperはfloor以上なのでそのまま動く（[persistence](../persistence.md#database-setup-and-migrations)）。
     - **引き継ぎ**（[Handoff](handoff.md#handoff)）: liveな登録それぞれに`request_handoff(token, <up自身の絶対path>)`を書き、各登録が同じtokenとpidのまま`binary_version`を`up`のbuild識別子にし、要求の列を消すのを`poll`ごとに待つ（`lifecycle::hand_off`）。何もsignalせず、LaunchAgentもcmuxのworkspaceも触らず、走っているrunのsessionもleaseもそのまま。待ちの上限は`--handoff-timeout`（既定1800秒。supervisorは進行中の検証と着地の区切りまで待ってからexecするので、integrateの`verification_commands`1回分に及ぶことがある）。要求を受けられない（`request_handoff`が拒む）、要求を拾う前に登録が消えた（同じpidが要求を書く前の時刻以降に`up`のbuild識別子で登録し直していれば、その新しいtokenで戻ったとする。候補が複数なら`started_at`の最も新しい登録で、要求より前に`started_at`を持つ同じpidとbuild識別子の古い行（pidの再利用で残ったもの）は後継とみなさない。`lifecycle::successor`）、heartbeatが30秒より古くなった（execした新しいbinaryが起動できなかった）、別のbuild識別子で戻った（execに失敗して前のbinaryで続けた）、上限までに戻らなかった、停止要求でdrainに入った（そのtokenの`supervisor_draining`がある。上限を待たずに、`is stopping (a stop request wins over the handoff) and was not handed off`の文面と`stopping: true`で失敗にする。[Handoff](handoff.md#handoff)）、のどれかでそのsupervisorを失敗にする。1つの失敗で待ちを止めず、上限（全員で1つ）の内で全員の成功か失敗を待つ（[ADR-t632-1](../../adr/2026-09-27-t632-1-handoff-restores-only-when-every-supervisor-failed.md)）。`hand_off`はsupervisorごとの結果（`lifecycle::Handed`: 元の登録、成功なら今のtoken、失敗なら理由）を返し、errorはqueueを読み書きできないときだけ。戻ったと見なすのはheartbeatが30秒以内の登録だけ（取り戻した直後に死んだプロセスを成功と数えない）。失敗したsupervisorの要求だけを、DBに残っていれば取り消す（`cancel_handoff`。要求の列はsupervisorがexec直前に`take_handoff`で原子的に受け取るか、登録の削除で消えるので、残っているのはまだ受け取っていない要求。drain中は各passで要求を読み直し、取り消されていればexecせずにdrainを解いて通常のclaimに戻る。残すと、呼び出し側が別のbinaryを置き戻した後でsupervisorがそのpathをexecする）。成功したsupervisorの要求は他の失敗で取り消さない。上限までに戻らなかった（上限の時点でまだ要求を拾っていなかった）supervisorの取り消しが要求を見つけなかった（`cancel_handoff`がfalse）ときは、上限の最後の見直しから取り消しまでのあいだに要求の列が消えた、つまりsupervisorがexec直前に要求を受け取ったか、登録が消えたので、失敗を確定せずにもう一度見直す: `lifecycle::HANDOFF_GRACE`（30秒。`--handoff-timeout`の方が短ければそれ）の猶予のあいだ`poll`ごとにqueueの登録を読み、上と同じ判定（`successor`と`look_at_handoff`）で、新しいbuild識別子で同じtokenか同じpidの後継として戻れば成功（今のtoken）にする。別のbuild識別子で戻った・heartbeatが古くなったならその理由で、猶予の内に戻らなければ`not back <N>s after the wait ran out`で、どちらも`took the handoff to <path> just as the wait ran out, but did not come back under <version>`と要求を拾った後に戻らなかったことが分かる文面の失敗にする。execの途中で登録が消えていて後継がまだ無いあいだは猶予の終わりまで待つ（同じあいだに`down`などで登録が消えたsupervisorも、猶予の後に失敗になる）。取り消せた（`cancel_handoff`がtrue）supervisorと、上限以外の理由の失敗は見直さない。1つでも失敗があっても（全員が失敗しても）、`up`はそこで止まらず、残りの段（`auto_update`の書き込み・結果の組み立て）を最後まで行ってから失敗する。fileを差し替えていないので戻すものは無い。errorは`lifecycle::PartialHandoff`（`install`の`KeptBinary`と同じ形）で、文面は`<N> of the <M> supervisors took the handoff to <version>; the ones that did not go on with the binary they had: `down --force` and `up` start them with this one`に、失敗したsupervisorごとのtokenとpidと理由を添えたもの、`report`は成功したときと同じ`up`の結果全体で、`supervisor.outcome`が`partially_handed_off`（全員が失敗なら`handoff_failed`）、`supervisor.replaced`の各要素に成功なら`previous_token`、失敗なら`error`が付く。`main.rs`はその`report`をstdoutに出し、errorのJSONをstderrに出してexit 1で終える。結果は（全員の成功で）`{"outcome":"restarted","handoff":true,"mode","version","previous_version","pid","token","workspace_id","plist","log_dir","replaced":[{"token","previous_token","pid","mode","workspace_id","version"}],"supervisor_workspaces":[]}`（`replaced`の`token`は今のtoken、`previous_token`は要求したときのtoken。`mode` / `pid` / `token` / `workspace_id`は先頭の登録のもの。プロセスが同じなのでmodeは変わらない。launchdへ移すときは`down --wait`と`up`）。`--no-wait`は引き継ぎでは何も変えない。
     - **drain**（引き継ぎを受けられない前のbinaryの登録、またはpathの違うlaunchd agentの登録）: live全部をdrainしてから1つ起動し直す。停止は`down --wait`と同じ順序: LaunchAgentを必ず外し（bootoutがSIGTERMを運び、同時に`KeepAlive`が古いbinaryを即座に立て直すのも止める）、`mode`が`in_cmux`の登録にはSIGINT、launchdがsignalしなかったプロセス（手起動、またはagentのPIDでないもの）にはSIGTERMを送り、その登録が全部消えるかPIDが死ぬまで`poll`ごとに待ち、`in_cmux`のworkspaceを閉じずに`left_open`と報告する（`down`と同じ`forget_supervisor_workspaces`）。それからlaunchd modeで起動する（入れ替えられる側がin-cmuxでも同じ）。待つ相手の除外集合はdrainの後に取り直すので、生き残った「生きているが黙っている」登録を自分の起動したものと取り違えない。結果は`{"outcome":"restarted","version":…,"previous_version":…,"replaced":[{"token","pid","mode","workspace_id","version"}],"supervisor_workspaces":[…]}`に、起動したsupervisorの`mode` / `pid` / `token` / `workspace_id` / `plist` / `log_dir`が並ぶ。drainに上限は置かない（runはClaude sessionなので待ち時間はrunの長さそのもの）。`up --no-wait`は待たないための逃げ道で、入れ替えるliveな登録のどれかのtokenがleaseを持つrun（`runs_leased_by`。状態を問わない）が1件でもあれば、件数とrun idと状態を挙げたerrorで止まる。runはreview・revise・e2e・着地のあいだ`awaiting_integration`のままleaseを保ち（ADR-0054 決定6）、resumeや復旧の回もleaseを取り直すので、`active_runs`の5状態だけでは担当中のrunを取りこぼす。逆に、drainが待たないものでは止めない: 誰もleaseを持たないrun（`task_runs.supervisor_token`に前の担当が履歴として残るだけ。答え待ちの`awaiting_integration`や再開待ちの`needs_session`など）、入れ替えないsupervisor（生きているがheartbeatの古い登録）のlease、どのsupervisorの登録にも無いtokenのlease（人の`dagq integrate`）。判定の文面は`lifecycle::no_wait_refusal`。判定はsignalもuninstallも何もする前に行うので、止まったときの状態は`up`を打つ前と同じ（PIDの死んだ登録のprune（2）だけは済んでいる）。入れ替えるsupervisorがleaseを持つrunが無ければそのまま入れ替えるが、`--no-wait`のときはdrainの待ちにも上限（`startup_timeout`、既定30秒）が付く: runが無くても止まらないsupervisorはありうる（loopがgitなどでhangしていてもheartbeat threadは別なので登録は新しいまま、判定とsignalの間にrunがclaimされることもある）ため、上限を超えたら残っているtokenとpidを挙げたerrorで止める（停止は既に頼んであり、agentも外れているので、`status`から消えたら`up`を打ち直す）。
   - **入れ替えの対象はliveな登録だけ**: PIDが生きていてheartbeatの止まった「生きているが黙っている」supervisorは、従来どおりreuseもpruneもkillもしないので、それが古いbinaryでも入れ替えられず、`up`はその隣に新しいversionのsupervisorを立てる。`status`がstaleとして報告するので、人が`down --force`で止めてから`up`をやり直す（[ADR-0014](../../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)のConsequences。[ADR-0045](../../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定16が引き継ぐ）。
   - **drainがPIDの死で終わったとき**: 登録が消えるのではなくPIDが死んで待ちが終わることがある（launchdの`ExitTimeOut`によるSIGKILL、heartbeat失敗であえて行を残す経路）。その登録行は`down --force`と同じようにここで消す。
   - plist: `~/Library/LaunchAgents/com.dagq.<queue hash>.plist`。`Label`は同名、`ProgramArguments`は`[up自身の絶対path, --db <db>, supervise, --log-dir <queue dir>/logs, --cmux <resolved>, --claude <resolved>, --codex <resolved>, ...]`（`--parallel N`は明示されたときだけ後ろに付く）（claudeは`up`がpreflightした絶対path（linkのまま）。launchdのPATHに頼らないため。cmuxは見つかればその絶対path、無ければ与えた値のまま渡し（supervisorは使わない）、`up`はcmuxが無くても止まらない。`--codex`（既定`codex`）も見つかればその絶対pathにし、無ければ与えた値のまま渡して止まらない。supervisorはCodexのtaskをclaimせず、`status`の`supervisors[].providers`が見つからなかったことを出す。[Provider lifecycle](../provider-lifecycle.md#workerのproviderと経路)）、`WorkingDirectory`はrepository root、`EnvironmentVariables`は`PATH`（`up`を叩いたshellのPATH）と、そのshellが`XDG_CONFIG_HOME`をexportしていたとき（空でないとき）だけ`XDG_CONFIG_HOME`（shellの値のまま。launchdのsupervisorが利用者の`config.toml`（[言語](language.md)）とhost全体の`host.toml`（[push](push.md)・[レポート](report.md)・[KPI](kpi.md)・[リリースの更新](release-update.md)）を、`up`を打ったshellと同じ`$XDG_CONFIG_HOME/dagq/`から読むため（`up`がpreflightで読む`config.toml`とずれない）。exportしていなければ入れず、supervisorは`~/.config/dagq/`を読む。testは`tests/it/lifecycle_up.rs`の`up_stores_the_exported_config_home_in_the_plist`と、無いときにPATHだけであることを`up_starts_the_agent_once_and_reuses_it_after_and_opens_no_inbox`）、`KeepAlive` true、`RunAtLoad` true、`StandardOutPath` / `StandardErrorPath`は`<queue dir>/logs/launchd.log`、`ExitTimeOut` 86400（launchdの既定20秒ではdrainが待てない）。生成は`src/infrastructure/launchd.rs`の`LaunchAgentSpec::xml`。
   - 起動: `launchctl bootout gui/<uid>/<label>`（未loadなら無視）の後に`launchctl bootstrap gui/<uid> <plist>`。既にbootstrap済みでも定義を差し替えるためbootoutしてからbootstrapし直す。bootoutは即返り、serviceは旧プロセスが終わるまで`launchctl print`に残り、その間のbootstrapはexit 5で失敗する（実機確認）ので、`print`が消えるまで0.5秒ごとに待つ。60秒で消えなければ`launchctl kill SIGKILL`を送り、さらに60秒待って諦める（hangしたsupervisorがlabelを`ExitTimeOut`の間占有しないため）。
   - macOSの外（launchdの無いhost）: `Launchctl`は`launchctl`を起動しない。`install`は「launchd mode needs macOS」のerrorで止まってplistを書かず、`down`と入れ替えのdrainはagentが載っていないとして進む（[Linux CI](../linux-ci.md)）。
     そのhostでは人かhostのservice managerが`dagq supervise`をforegroundで動かす（ADR-t1433-4決定4）。
   - 登録が30秒以内に現れなければerrorで止め、agentはそのまま残す（`launchd.log`を見る）。preflightに通らない環境ではKeepAliveで再起動を繰り返すので、`down`で外す。
4. **inbox**: `up`はinboxを開かない（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定3）。
   inboxは人が自分のterminalで`dagq inbox`を打って開く（下の「`dagq inbox`」）。
   `up`はplannerも開かない（plannerはruntimeが立て、人が開く`dagq plan`は廃止した。[`plan` / `planners`](plan-planners.md#plan--planners)）。
   `up`は`session_workspaces`のうちsupervisor以外のroleの行（前のbinaryの`up`が開いたinboxの行、退役した常駐sessionと常駐plannerの行）を、cmuxを呼ばずに消す。
   workspace自体は閉じず、人が閉じる（決定6）。
   `up`を打った環境の`DAGQ_ROLE`と`DAGQ_QUEUE`は見ない。
5. **結果**: `{"supervisor": {"outcome": "started"|"reused"|"restarted"（引き継ぎの一部か全員が失敗したときはerrorの`report`で`partially_handed_off`|`handoff_failed`）, "mode": "launchd"|"in_cmux"|null, "version", "pid", "token", "workspace_id", "plist", "log_dir"}（`restarted`のときは`previous_version` / `replaced` / `supervisor_workspaces`も、引き継ぎなら`handoff: true`も）, "migrated": <適用した互換のmigrationの`migrate`の報告、無ければnull>, "inbox": {"outcome": "not_opened", "next": <`dagq inbox`で開く案内>}（`--no-claude`なら`{"outcome": "skipped", "reason": "provider_disabled", "next"}`）, "retired_sessions": <忘れたsession_workspacesの行数>, "pruned_supervisors": [{"token","pid","reason"?}], "doctor": {"unfinished_runs": [{"run_id","task_id","status","lease_stale"}], "awaiting_integration": [{"run_id","task_id","last_error"}], "needs_session": [...]}}`。`lease_stale`はleaseのPIDが死んでいるかheartbeatが30秒より古いとき`true`、leaseがなければnull。

前提: supervisorはcmuxを呼ばないので（[ADR-t1433-4](../../adr/2026-10-03-t1433-4-supervisor-resides-without-cmux.md)）、launchdが起動するsupervisorにcmuxのsocket passwordは要らない。`up` / `down` / `dagq inbox`もcmuxを呼ばず、cmuxの無いPATHで動く（ADR-t2159-1決定1）。

## `dagq inbox`

`dagq inbox [--plugin-dir PATH] [--repo PATH] [--claude EXE] [--codex EXE]`は、inboxを打った人のterminal（種類を問わない）の前面で開く（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定2）。
queueは`up`と同じく`--db`かcwdのrepositoryから解決する。
inboxのproviderはClaude Codeで、Codexはinboxのcommandを持たないので`--codex`は受け付けるだけ。

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
   - **見せる欄**: `status`（role無しと`--role inbox`）と`doctor`の`inbox_guardrail`は、最新の`inbox_opened`で判定する。
     前のbinaryの`up`が開いたinboxの記録も同じく判定する。
     guardrailが無ければ、引き継ぎを書き、`DAGQ_ROLE`の無いterminalでinboxを終えて`dagq inbox`で開き直すよう案内する（手順の正本はpluginの`dagq-recover`の`reference/up-down.md`の「Open the inbox again」）。
   - **限界**: settingsはClaude Codeの起動の引数なので、inboxのterminalで`claude`を打ち直したsessionと、`dagq inbox`を通さずに開いたinboxには効かず、見えない（ADR-t2159-1決定5）。
     denyはtoolの呼び出しの綴りでの照合で、絶対path・scriptの中・別のshellからの呼び出しは止めず、enforcementではない（[Security](../security.md)）。

`dagq down [--wait] [--force]`はsupervisorを止める（`--cmux`は前のcommand lineのために受け付けて使わない）。
launchdのものも、登録済みのin-cmuxのものも、手で起動したものも同じ1コマンドで止まる。
まずPIDの生死で登録を分ける。
PIDが生きていてもheartbeatが30秒より古い登録は、`up`と共通の`pid_taken_over`でこのuserのプロセス一覧と開始時刻を調べ、別プロセスに取られたと分かれば登録を消して`pruned_supervisors`に`reason: "pid_reused"`を出す。
このPIDには通常のsignalも`--force`のSIGKILLも送らない。
プロセス一覧を読めないときとheartbeatが新しいとき、および登録より前に起動した止まったsupervisorは従来どおりsignalの対象にする。
次にLaunchAgentを必ず外す（`launchctl print`でagentの有無とPIDを読み、`launchctl bootout gui/<uid>/<label>`してplistも消す。`RunAtLoad`のため残すと次のloginで復活する）。
signalの対象の登録が1件もなければ`{"outcome":"not_running","launch_agent_unloaded":…}`（`--force`ならPIDの死んだ登録行も消して`pruned_supervisors`に出す）。
あれば登録ごとにsignalを選ぶ: `mode` が`in_cmux`ならSIGINT（cmuxのterminalでCtrl-Cを押したのと同じ。runtimeはSIGTERMと同じくdrainに入る。このmodeにはsignalを届けてくれるservice managerがない）、そうでなければ従来どおりSIGTERM——ただしbootoutでlaunchdがagentのプロセスにSIGTERMを届けるので、agentのPIDには送らない（runtimeは1回目のsignalでdispositionを既定に戻すので、2回目は即死になる）。
agentがloadされているのにPIDが読めないときは誰にも送らない。
既定は`{"outcome":"draining","pid":…,"pids":[…]}`で即返り、`--wait`はその登録が消えるかPIDが死ぬまで2秒ごとに待って`stopped`、`--force`はSIGKILLを送って登録行を消し`killed`（leaseは30秒でstaleになり、runは`doctor` / `recover`で扱う）。

queueのbroker（未実装）はqueue serviceと同じ`up` / `down` / execの引き継ぎで起動・停止し、podmanでは動かさない（[ADR-t2114-1](../../adr/2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)）。

`down`はcmuxのworkspaceを閉じない（[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定3・6）。
登録済みのin-cmuxのsupervisor（前のbinaryがそのmodeで登録したもの）が動いていたworkspaceは、結果の`supervisor_workspaces`に開いたまま残すと出して人に任せる。
`session_workspaces`の`supervisor`行はcmuxを呼ばずに消す。
どの経路（`not_running`・既定・`--wait`・`--force`）でも`supervisors`の全登録が対象で、signalの順（in-cmuxにはSIGINT）と登録の後始末は変えない。
inboxは人のterminalで、`down`は触らない。

## CIの見張り（ADR-t1920-1）

[ADR-t1920-1](../../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](ci-watch.md)。`[ci_watch]`があれば、`up`のpreflightは`[run.env]`のプログラムの検査の後に（`lifecycle::Ports::ci_watch_preflight`、`infrastructure::ci_watch::preflight`）、`up`のPATHで`gh`を解決し、`[repository] remote`のURLがGitHubかと`gh auth status`を確かめ、だめならsupervisorを起動せず、理由と対処（`gh`を入れる・`gh auth login`・remoteを直す、または`[ci_watch]`を`dagq.toml`から外すtaskを登録する）を挙げ`; the supervisor was not started`で終わるerrorで止まる。
