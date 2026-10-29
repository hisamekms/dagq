---
id: design-supervisor-lifecycle-run-environment
type: design
title: "Run environment"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-0072
  - adr-t1920-1
  - design-supervisor-lifecycle-ci-watch
  - adr-t1857-1
  - adr-t963-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-integrate
  - design-supervisor-lifecycle-language
  - adr-0040
  - adr-0076
  - adr-t1453-1
  - adr-0049
  - adr-0079
  - adr-t1215-1
  - adr-t2086-1
  - design-supervisor-lifecycle-worker-model
  - design-supervisor-lifecycle-actor-model
  - adr-t2268-1
  - design-toolchain-profiles
  - adr-t3321-1
---

# Run environment

repository rootの`dagq.toml`の`[run.env]`（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定3）が、runごとの環境変数になる。
読み込みの入口は`src/infrastructure/run_env.rs`の`load_run_env`で、ファイルが無ければ空。

- 書式はTOMLの部分集合で、表は`run_env::parse_config`が受け付けるものだけを持ち、`[run.env]`の各行は`KEY = 'literal'`か`KEY = "basic"`（escapeと拒むkeyは`run_env::parse_config`のdoc comment）。
  `#`以降はcomment。
  ほかの表（外したresource brokerの`[broker]`と`[broker.package]`は中を問わず読み飛ばし、残したrepositoryを止めない。ADR-t2125-1）、重複したkey、`DAGQ_`で始まるkey（runtimeが`DAGQ_ROLE` / `DAGQ_QUEUE`に使う）はエラーにする。
  下の各表でも（中を読まない`[language]`を除く）、知らないkey・keyの重複・型や範囲の合わない値は行番号付きのエラーにする。
  旧バイナリは知らない表とkeyを含む`dagq.toml`全体を読めなくなり本番のqueueのrunがすべて止まるので、下の表とkeyは固定バイナリが読めるようになってから足す。
- 読むのはrepositoryのmain checkout（下の「main checkoutの決め方」）の作業ファイルで、run worktreeのものではない。
  検証コマンドが1件も無ければ読まない。
- 渡し先: (a) `provision`がworkerのsession wrapperをbackgroundで起動するとき、`DAGQ_ROLE`ほかのworkerの変数の後ろにwrapperのprocessのenvとして足す（ADR-0026の仕組みを[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)決定3でprocessのenvと読む）。
  worktreeを作る前に読むので、壊れた`dagq.toml`はprovisioningの失敗になり、wrapperは起動せずsupervisorはclaimを止める。
  (b) `integrate`と`already_satisfied`の検証の`verification_commands`を`Command`のenvに足す（それ以外のvalidatingは流さない）。
  hostの検証（`integrate`のflakyの流し直しと`already_satisfied`を含む）は`TMPDIR`を`runs/`の外の短いrunごとのdirで置き換え（socketのpathの長さと`runs/`の下を見るtestのため）、containerの検証は置き換えない（[Run worktrees](run-worktrees.md#終わっていないrunのビルド成果物)）。
  読めないファイルは着地処理のエラーで、runは元の状態に戻る。
  (c) reviewのheadless実行（ADR-0049の決定2）のコマンドのenvに足す。
  (d) `needs_session`のresumeが起動するwrapperのprocessのenvに、(a)と同じくworkerのenv（`DAGQ_ROLE` / `DAGQ_QUEUE`とworkerのactorの名前）の後ろに並べる。
  resumeのsessionはworkerと同じ手元の検証（fmt・clippy・関係するtest・e2e）を回すので、workerと同じ`[run.env]`（sccache、buildとtestの並列度）が要るため。
  resumeのsessionはworkerのsessionの続きで、ADR-0049の決定3の渡し先の「workerのworkspace」に含まれると読む。
  値はresumeの試行を始める前に読み、landing branchと同じく読めなければ試行を使わずrunは`needs_session`のまま（landing branchの読み込みが同じfileを先に検査するので、壊れた`dagq.toml`はそこで止まる）。
  `[run.env]`が名指すプログラムが見つからない間は、claimと同じくresumeも始めない（下の「`[run.env]`が名指すプログラムの検査」。始めるとsessionのcargoがすべて失敗して試行を使うため）。
- `dagq.toml`はrepositoryにcommitされ、値はworkerのwrapperとturnのprocessのenvに渡るので、secretは入れない。
- この repositoryの`[run.env]`の値と理由、置かないものとsccacheの入れ方の規則は`dagq.toml`の`[run.env]`のコメント、`.config/nextest.toml`に並列度を書かない理由はその冒頭のコメント、その補足とほかのhostのツールの入れ方は[運用の開発文書](../../development/operations.md)の「`[run.env]`とtestの並列度の置き場」と「hostのツール」が持つ。
- cargo-nextestは`[run.env]`が名指すプログラムではないので下の検査の対象ではなく、無いhostでは着地の検証の失敗になる（[ADR-t1925-1](../../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)の決定8）。
- `[recheck]`は着地のたびにsupervisorが着地待ちのrunをmainに載せた木で実行する軽い検査で、keyの意味は`domain::recheck::RecheckConfig`、流れとcommandに渡るenvは`supervise::execution::recheck`が持つ（[ADR-0068](../../adr/0068-recheck-waiting-runs-after-each-landing.md)の決定2、[ADR-t2032-1](../../adr/2026-10-07-t2032-1-run-the-recheck-command-only-on-runs-touching-recheck-paths.md)）。

- `[review.subagents.<agent>]`はreviewのsubagentを必須にするglobで、`paths`（`--paths`と同じglobの配列）と任意の`provider`（[Review](review.md#agentのjobのprovider)）を持つ（ADR-t1453-1の決定1）。
  - `<agent>`はkebab-caseで、定義のファイル`.dagq/agents/<agent>/AGENT.md`のdirectoryの名前になる（[ADR-t1728-1](../../adr/2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)）。
  - `builtin:`で始まる名前はbuilt-inのagentの表で、任意の`enabled`・`paths`・`provider`を持ち、書いた`paths`は既定を置き換え、書かないものは既定に従う（意味と既定は`domain::review_builtins`、[ADR-t2259-1](../../adr/2026-10-12-t2259-1-builtin-review-agents-and-programs-configured-by-builtin-names.md)）。
  - 書き方の誤り（`domain::review_builtins`と`run_env`）は`dagq.toml`全体の誤り。
  - 定義の無いagentと複数の役割の節が名指すagentは`dagq doctor`の`agents`の`errors`が出す（reviewでの扱いは[Review](review.md#reviewのsubagent)の「誤り」）。
  - 読むのはreviewを始めるときのlanding branchのcommitの`dagq.toml`で、選び方とsnapshotは[Review](review.md#reviewのsubagent)。

- `[disk]`はclaimと着地の検証の前に確かめる空き容量の閾値で、keyと既定は`domain::disk::DiskConfig`、読む時と読めないときの扱いは[claimを控える](claim-hold.md#空き容量)が持つ（ADR-0047の決定44）。

- `[resume]`は`needs_session`のrunの衝突だけの試行の上限で、keyと既定、設定にしない上限は`domain::resume::ResumeConfig`が持つ（[Needs session](needs-session.md)の「試行の数え方」、ADR-0047の決定24）。
  supervisorが起動時に読み（`SuperviseOptions::resume`で上書きできる）、読めなければ既定値で動く。

- `[retention]`は終わったplanner・plan review・observationのdirを残す日数`days`と`backups/`の数`backups`で、意味は`RetentionConfig`（ADR-t697-1）。

- `[supervisor]`はsupervisorの枠と控えの設定で、keyと意味は`domain::slot_limits::SupervisorConfig`が持つ（[人の答えを待つrun](waiting.md)・[Draft planners](draft-planners.md)・[Finding planners](finding-planners.md)・[Plan review (supervisor)](plan-review.md#plan-review-supervisor)・[claimを控える](claim-hold.md)・[実行区分の上限とaging](claim-hold.md#実行区分の上限とaging)、ADR-t1479-1、ADR-t1591-1）。
  projectごとのbuildの重さに合わせた並列数と、答えを待つplannerが枠をふさいでもdraftとreviseが進む数と、buildのloadが出るまでの間をrepositoryに置くため。
  - **優先順**: flagの明示 > `[supervisor]` > 既定で、keyごとのflagの有無は`domain::slot_limits`のdoc commentが持つ。
    `up`はflagを明示されたときだけ`supervise`の引数に渡し、明示が無ければ焼き込まない（[`up` / `down`](up-down.md)）ので、`up`で起動したsupervisorもこの順で決まる。
  - **効くとき**: supervisorが起動時（引き継ぎのexecの後も）に読み、続けてループの各passで`[conflicts]`の読み直しの後に読み直す（[`supervise`](supervise.md)）。
    読み直した値が使っている値と違えば、そのpassから新しいclaimと待ちとplannerを立てる判定とclaimの間隔に使い、登録の値と出どころを書き換え、queue event `supervisor_config_changed`を記録する（`light_changes`だけが変わったときも記録し、登録は書き換えない）。
    main checkoutに着地した変更は`up`し直さずに効く。
    下げた値を超えて走っているrunと待っているrunと開いているruntimeのplannerはそのまま続き、下回るまで新しいclaimと待ちとplannerが控えられる。
    表を消すと既定（`default`）に戻る。
    ファイルが無いか読めない・値が誤っているときは使っている値のまま続ける（起動時に読めなければ既定値で起動する。どちらもlogにwarnを出す）。
    ただし誤った`dagq.toml`は`[repository]`などの読み手も止めるので、claimと着地も止まる。
  - **見え方**: `status`と`doctor`は各値とその出どころを出す（欄は`domain::views`のdoc comment）。
  - 起動し直す`up`: `install --allow-breaking`のdrain（`install::restart_arguments`）と`install`の見張りの戻し（`SupervisorRegistration::flag_arguments`）が打ち、updateのjobは打たない（[Auto-update](auto-update.md)の5）。
    引き継ぎのexecは元のargvのまま続くので、flagを付けて起動したsupervisorは入れ替わっても`flag`のままで、`[supervisor]`に従わせるには`down --wait`の後にflagなしで`up`する。
- `[provider_fallback]`は使えないproviderからもう一方への切り替えを止める設定で（[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)。`domain::provider_switch::ProviderFallback`）、`workers`と`jobs`を受け付ける。
  振る舞いと優先順位は[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)の「切り替えを止める設定」。
  読み手は`compose::supervisor`で、supervisorが起動時に読み（読めなければ既定で起動する）、各passで`[supervisor]`の後に読み直す（再起動なしで効く。ファイルが無いときは黙って、読めない・値が誤っているときはerrorごとに1回logにwarnを出して、使っている値のまま続ける）。
- `[fresh_session]`は`peak_context_above`（意味は`domain::fresh_session::FreshSessionConfig`）だけを持つ（[ADR-t2080-1](../../adr/2026-10-08-t2080-1-fresh-session-on-send-back-and-resume-when-peak-context-exceeds-threshold.md)、振る舞いは[Provider lifecycle](../provider-lifecycle.md#大きなcontextからの新しいsession)）。
  keyが無ければ新しいsessionを始めない。
  値とkeyの誤り、読む時と読めないときの扱い（読み手は`load_fresh_session`）は`[provider_fallback]`と同じ。
- `[replan]`は長期化したtaskの検知と再計画の診断の設定で、表が無いか`enabled = true`でなければ無効で何も検知・診断しない（keyと既定は`ReplanConfig`のdoc comment、流れは[taskの再計画](../../plans/task-replanning.md)）。
  supervisorが各passにmain checkoutから読む。
  旧バイナリは知らない表で`dagq.toml`全体を読めなくなるので、固定バイナリが読めるようになってから足す。
- `[eval]`と`[eval.providers.<provider>]`はagentのevalの上限・閾値・費用の見込みと採用した定義の見張りの時期で、keyと既定値は`EvalConfig`・`ProviderCost`のdoc comment、使い方は[agentのeval](../agent-eval.md)が持つ。
- `[review.jobs]`はrunのreviewの段のjobの種類ごとの時間の上限と、reviewのagentのjobの同時実行の上限で（ADR-t1895-1決定1。keyと既定値は`ReviewJobs`のdoc comment）。
  他のjobに効かず（evalのケースのprogramのjobはreviewの段に数える）、無いkeyは今の上限（[Headless job processes](headless-job-processes.md#記録)）。
  supervisorが起動時に読む。
- `[review.programs.<name>]`はreviewのprogramのjobで（[プログラムのreview](review-programs.md)）、keyと意味は`ReviewProgram`が持つ。
  `[review.programs."builtin:<名前>"]`はbuilt-inの`enabled`と`paths`（既定を置き換える）だけを持ち（`BuiltinProgramSetting`）、ほかのkeyは誤り。
  読むのは作業ファイルでなく、reviewの試行ごとのlanding branchのcommitの`dagq.toml`と`script`（`Verifier::review_programs_in`）。
- `[review.code]`は上限の後の判定（[Review](review.md)の4a）のコードのpath、`[review] revise_limit`はrunを通した手直しの上限（ADR-t3033-1）で、意味と既定は`review_reason::CodePaths`・`ReviseLimit`が持つ。
  どちらも読むのはreviewの試行の始めのlanding branchのcommitの`dagq.toml`で、作業ファイルとrunのbranchは読まず、読めなければ既定に戻さず試行をreviewの失敗にする。
- `[tasks]`は`changes`と`title_prefixes`と`related_docs`を持つ（値と形は`domain::ChangeSet`・`domain::TitlePrefixes`）。
  `changes`はtaskが宣言するchangeの値の集合で（[ADR-t980-1](../../adr/2026-09-29-t980-1-classify-runs-by-declared-change-and-diff-derived-area.md)）、書いてあれば`add`と`edit`は集合の外の`--change`を拒み、`lint`は指摘し`submit`はchangeの無いtaskと集合の外のchangeを拒む（`submit`は何も動かさない）。
  `title_prefixes`はtitle（mainのsquash commitの件名）の接頭辞の一覧で、書いてあれば`lint`は指摘し`submit`はそれで始まらないtitleを拒むが、`add`と`edit`は拒まない。
  読むのはqueueが束縛されたmain checkoutの`dagq.toml`で、上のコマンドのたびに読み、`dagq.toml`のどこかが解析できなければそれらもそのerrorで止まる。
  書いてなければchangeは形だけを検査して省略でき、titleは見ず、hintsは出ない。
  `related_docs`は`lint`のhintsが照らす文書のglobで、`lint`だけが同じcheckoutから文書も読む（ADR-t3321-1、`domain::doc_hints`）。
  runtimeが自分で登録するdraft（follow_upなど）は検査しないので、planは`lint`と`submit`で揃える（titleは`edit`で直す）。
- `[goals]`は`tags`と`max_spike_cycles`を持ち、goalのラベル（tag）の値の集合を1行の配列で書く（[ADR-t1639-1](../../adr/2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)の決定6。`domain::TagSet`）。
  値は`GoalTag`の形（[domain-model](../domain-model.md)）で、形の合わない値と重複は誤り。
  書いてあれば`goal add`と`goal edit`は集合の外の`--tag`を拒み、`lint`は所属のgoalがラベルを持たないか集合の外のラベルを持つdraftのtaskを指摘し、`submit`は与えたgoalと動かすtaskの所属のgoalのうち閉じていないdraftのgoalがラベルを持たないか集合の外なら何も動かさずに拒む。
  読み方は`[tasks] changes`と同じ。
  書いてなければ形だけを検査し、goalはラベルを持たなくてよい。
  `max_spike_cycles`はgoalのSpikeの周期（Spikeとその結果の再計画の回）の上限で、`submit`だけが読む（`investigation::spike_submit`）。
  値は`dagq.toml`、各ラベルの意味と付け方は[taskの登録](../../development/task-registration.md#goal-の優先度とラベル)が持つ。
- `[e2e]`は`paths`だけを持ち、変えるとrunに`e2e`のevidenceを求めるpathをglobの1行の配列で書く（ADR-t963-1決定2。`Verifier::e2e_paths`）。
  読むのはsupervisorがvalidatingとworkerのpromptを作るたびのmain checkoutの作業ファイルで、読めなければ警告を出して空として扱う（本番を守るのは[Auto-update](auto-update.md)の入れ替え前のe2eの関門）。
  使い方と無いときの扱い、この repositoryの範囲は[Validation](validation.md)の8と「この repository で置く範囲」。
- `[local_checks]`と`[local_checks.gates.<name>]`は手元の関門の宣言で、reviewがmainのcommitから読む（ADR-t2589-1決定6、`run_env::LOCAL_GATES_PREFIX`）。
- `[areas]`は`stats`と`kpi`がrunを分けるareaの名前→globの1行の配列（[ADR-t980-1](../../adr/2026-09-29-t980-1-classify-runs-by-declared-change-and-diff-derived-area.md)。`domain::areas::AreaMap`）。
  書式は上の型、求め方は`application::areas::AreaReader`のdoc commentが持つ。

- `[kpi]`と`[kpi.targets."<KPI>"]`はKPIの判定の設定と目標で、KPIの計算（`kpi`・レポート・push・observerの入力）だけが読む（[計測のコマンド](commands.md)、[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定17・19。解析は`src/infrastructure/kpi_config.rs`の`KpiTables`）。
  `[kpi]`の設定の数値は既定のまま書かない。

- `[headless]`は`wrapper`だけを持ち、`"workspace"`か`"background"`を書く（ADR-t1404-1決定7）。
  欄は受け付けるがworkerにもruntimeのplannerにも効かず、wrapperは値に関わらずbackgroundで起動する（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)決定2、[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)、[Plan planners](plan-planners.md)）。
  値が`"workspace"`なら、supervisorはworkerに効かないことをprocessごとに1回だけ警告する（`Verifier::headless_wrapper_setting`）。
  欄を消すのは、古い固定バイナリが欄の無い`dagq.toml`を読めることを確かめた後にする。
- `[worker.trial]`はworkerのmodelの限定の試しの設定で、keyと既定は`domain::worker_model::WorkerTrial`が持つ（[Worker model](worker-model.md)、[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)の決定4）。
  supervisorがclaimのpassごとに読み（`Verifier::worker_trial`）、読めなければ試しの外でclaimする。
  taskに由来する失敗の後のworkerのsessionの段上げ（決定5、[Worker model](worker-model.md#段上げ決定5)）は設定を持たず、`[worker.trial]`の有無に関係なく働く。

- `[roles.<role>]`はworker以外のアクターのsessionのprovider・model・effortを持つ（[Actor model](actor-model.md)、[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)の決定7、[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)の決定1）。
  役割・key・値と読み込みが拒むものは、`domain::actor_model`のdoc commentが持つ。
  読み込みの入口は`Verifier::role_models`で、読む時点と、読めないときと表の無いときの起動は[Actor model](actor-model.md)が持つ。
  runtimeのplannerの古い経路の欄は受け付けて無視し、runtimeのplannerは常に非対話で動く（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)の決定3）。
  その役割をCodexで動かす実装を持たないバイナリはCodexを拒むので、各節は固定バイナリがその役割の実装の着地commitを含むことを切り替えのverifyの関門で確かめてから足す。
- workerとintegrateのverificationを動かす場所の設定は、綴りと値を`RuntimeConfig`のdoc commentが、読み込みの入口と流れを[Container runtime](../container-runtime.md)が持つ。
- `[container.image]`はimageに足すprojectのContainerfileと入力を名指し、keyの綴りと意味は`ProjectImage`のdoc commentが、buildと読む場所は[Container runtime](../container-runtime.md#image)が、案は[profile](../toolchain-profiles.md#推奨の設定)が持つ。

- `[language]`はAIが人に向けて書く文の言語（`tag`、BCP 47の言語タグ）を持ち、利用者ごとの`config.toml`の同じ表より優先する（[Language](language.md)、[ADR-t616-2](../../adr/2026-09-27-t616-2-language-of-text-ai-writes-for-people-is-configurable.md)）。
  表は`run_env::parse_config`が中を見ずに受け付け、誤りでclaimや着地を止めないよう[Language](language.md)の読み手が検査する。

- `[repository]`は着地先のbranch・pushのremote・pushするかを持ち、欄と既定は`domain::landing_branch::RepositoryConfig`が持つ（[ADR-t615-1](../../adr/2026-09-27-t615-1-landing-branch-and-push-remote-per-repository.md)）。

- `[ci_watch]`はsupervisorが着地先のbranchのCIの結果を`gh`で読む見張りの設定で（[ADR-t1920-1](../../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)。振る舞いは[CI watch](ci-watch.md)）、表があるときだけ有効になる（表が無ければ無効で、何も読まず何も記録しない）。
  keyの型・既定・意味は`domain::ci_watch::CiWatchConfig`のdoc commentが持ち、読み直しと`up`のpreflightは[CI watch](ci-watch.md)の「いつ確かめるか」と「読む手段が無いとき」が持つ。
  この repositoryの値は`dagq.toml`の`[ci_watch]`が持ち、`ci.yml`で成果物やRustのjobの名前（`required_jobs`、[CI watch](ci-watch.md)の「実行の扱い」）を変えたら同じ変更で合わせる。
- `[landing_verification]`は`integrate`がtaskの検証のコマンドの一部をrepositoryの着地の検証のコマンドに置き換える設定で（[ADR-t1925-1](../../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)。流れと境界は[Validation](validation.md#着地の検証)）、表があるときだけ働く。
  `integrate`が着地のたびにmain checkoutから読む（`Verifier::landing_verification`。読めなければ`[run.env]`と同じく着地処理のエラー）。
  書式（key・型・既定値・検査）と、置き換えたコマンドにだけ`[run.env]`の後ろに足すenvの意味は`domain::landing_verification`のdoc commentが持つ。
  そのenvは`DAGQ_`で始まり、`[run.env]`は`DAGQ_`のkeyを拒むので衝突しない。
  何を置き換え、ITをどう選ぶかはrepositoryの設定とscriptが持ち、runtimeは知らない。
- `[egress]`の`registries`は、containerのrunが[queueのbroker](../queue-broker.md)を通って届くregistryのhostを挙げる（[ADR-t2257-1](../../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定3）。
  書式・意味・既定は`run_env::EGRESS_REGISTRIES`のdoc commentが、案は[profile](../toolchain-profiles.md#推奨の設定)が持ち、brokerは`CONNECT`のたびにmain checkoutから読む。
- `[toolchains.<name>]`は使うtoolchainのprofileと当てるpathsの宣言で、宣言したprofileはpathsに依らず`integrate`とlanding recheckの検証の出力の読み方に効く（書式と誤りの扱いは`run_env::TOOLCHAINS_PREFIX`のdoc comment、当て方は[Toolchain profiles](../toolchain-profiles.md)）。
- `[verify_marks.<name>]`は検証の出力のprojectに固有の印で、既存の分類に印を足し、宣言したprofileに依らず`integrate`とlanding recheckに効く（書式は`run_env::VERIFY_MARKS_PREFIX`のdoc comment）。

## sccacheのserver

hostのsccacheのserverはsupervisorがsandboxの外で起動して持ち、runtimeが`[run.env]`を渡すほかのprocessには、sandboxの中でも外でも起動させない（[ADR-t1215-1](../../adr/2026-10-02-t1215-1-supervisor-owns-the-sccache-server.md)、[ADR-t2086-1](../../adr/2026-10-08-t2086-1-no-runtime-process-starts-the-sccache-server.md)）。
clientが起動したserverはclientのsandboxを引き継いで他のrunのbuildも失敗させ、起動の記録も残さないため。
適用先は渡し先(a)〜(d)と(a)の開き直し（providerを問わない）、着地前の再確認とe2eのコマンド。
適用先には、どのprocessも開けない`SCCACHE_ERROR_LOG`を渡す（supervisor自身の起動には渡さない）。
clientは自分を起動し直してserverにし、その子はlistenの前にこのlogを開いて終わるので、途中でserverが止まっても適用先からは起動されない。
開始の直前にserverを確かめたものは、`RUSTC_WRAPPER`をguard（dagqへのlink）にする。
guardはcompileごとにportを確かめ、listenしていれば`[run.env]`のsccacheを通してcacheを使う。
listenしていないとき、確認の直後に止まったserverの起動を拒まれてclientが自分のerrorで失敗したときは、compilerを直接実行する。
serverを確かめられないもの、guardを作れなかったもの、containerのverificationはwrapperを外してcacheなしでbuildを通し、eventに残す。
supervisorが始めるものの前の確認は、居ないserverを起動する機会でもある。
hostのserverを、wrapperのturnの前・人の`integrate`・e2eのgateの確認とguardはportを見るだけで起動しない。
runtimeの外からの起動までは防げず、下の検知が扱う。
workerのcontainerは`[run.env]`がsccacheを名指すときだけ、supervisorがagentより前にcontainerの中に起動したserverを使い、portとcacheのdirは起こした時のcontainerのenvとmountから取る（`domain::sccache::ContainerServer`）。

### 外部serverと失敗の偏り

起動の記録と現在のidentityが一致しなければ、serverの出どころは不明として扱う。
起動の記録が起動時刻を持たないときは、pidとportが一致し、processの起動時刻が記録の`at`と合えばその記録のsupervisorを起動者とする（照合は`application::sccache::owner`）。
親processは付け替わるため、sandboxの印が読めないことはsandboxの外で起動した証拠にならない。
既存の失敗率だけで壊れたserverとは決めず、同じidentityの新しい失敗の偏りか、確かめられたsandboxの印で判断する。
成功なしの新しい失敗の差分が閾値（`domain::sccache`のdoc comment）に達すれば壊れた印とし、成功・identityの変更・統計のリセットで取り直す。
processや統計を読めないことだけでも壊れたものとは決めない。
統計を読む前に存在を確かめ、診断の読み取りでserverを起動させない。

再起動の失敗は次のsupervisorへ引き継ぐが、別のidentityを古い故障の理由で止めない。
同じidentity・program・errorの再起動の失敗は、supervisorが起動を記録するまで再通知せず、再試行は続ける。
同じidentityと理由の壊れた印も繰り返し記録せず、故障の変化とsupervisorの起動を区別する。
再起動に失敗した知らせを受けた人はdoctorでidentityとportを確かめ、hostでserverを止め、起動をsupervisorに任せる。
supervisorが置換の起動を記録すると知らせが消え、人が別のserverを起動して検知されただけでは消えない。

- 監視と再試行の入口は`supervise::host_ops::sccache`。
  drain中も監視し、待ちの経過は注入した時計で測る。
- 判断・記録と確認・hostへの問い合わせ・guardのlinkは各層の`sccache`のmoduleが持つ。
  再起動の失敗の通知は、稼働中とserver不在の両経路から同じ照合を通る。
  guardの適用はturnが`application::headless_session`、ほかは`[run.env]`を組む各所（review・`integrate`・recheck・着地の前のe2eの関門）。
- doctor/statusの表示条件と読み取りは`application::sccache::add_diagnostics`。
  statusは統計clientを呼ばず、doctorが統計を読む。

## main checkoutの決め方

`dagq.toml`を読むmain checkoutは、repositoryのmain worktree（`git worktree list --porcelain`の最初のentry）で、決めるのはGitで`main_checkout_of`、Gitを起動しないqueue serviceでは束縛から`main_checkout_from_files`の2か所だけ（bareと`git init --separate-git-dir`などの構成の扱いはそれぞれのdoc comment）。
`dagq.toml`を読むコマンドとaskの通知のrepositoryの名前はどれもこの結果を使う。

main checkoutが無いとき、実行したcheckoutやrun worktreeの`dagq.toml`で代わりにしない（runごとに設定が変わりうるため）。
`up`はpreflightで止まってsupervisorを起動せず、`supervise`と`integrate`は起動時に同じerrorで止まり、`doctor`の`repository`は`error`を出して`run_env`は出さない。
`dagq plan`はmain checkoutを読まずに拒む。
`--from`なしの`install`は`--from`を求めるerrorになる。
inboxのwatchのaskの通知は、queueが束縛されたcommon directoryの名前で出す（束縛の無いqueueでは実行したcheckout）。

## `[run.env]`が名指すプログラムの検査

[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定8・9。`[run.env]`の`RUSTC_WRAPPER`などが実行できないと、runのcargoは`could not execute process`ですぐ失敗し、そのまま流すとworkerの検証も`integrate`の検証も全部落ちてresumeを使い切る。そこでruntimeが、cargoを走らせる前に解決できるかを見る。

- **検査する変数**: cargoがプログラムとして実行する変数（`src/infrastructure/run_env.rs`の`PROGRAM_VARIABLES`）だけで、それ以外の変数は見ない。
- **解決の規則**（入口は`check_run_env_programs`）: 値が空なら検査せず、`/`を含む値はそのpathが実行可能なfile、含まない値は検査するプロセスのPATHの各directoryに実行可能なfileがあること。
  `${DAGQ_QUEUE_DIR}` / `${DAGQ_RUN_DIR}`は展開してから見る。
  runの無い検査（`up`・claim・`doctor`）では`${DAGQ_RUN_DIR}`を含む値は検査しない（runの前にはrun directoryに何も無いため）。
  `dagq.toml`が無ければ何も検査しない。
  結果は`domain::run_env::RunEnvCheck`。
- **`up`のpreflight**: Claude・trustの後に、`up`のPATH（supervisorに渡すPATH）で検査する（`lifecycle::Ports::run_env_programs`）。
  見つからなければsupervisorを起動せず、変数名・値・PATHと対処（入れるか、`dagq.toml`から外すtaskを登録する）を挙げたerrorで止まる。
  `dagq.toml`があるrepositoryでは`up`の出力に検査の結果が付く。
- **supervisorのclaimの前**: 各fill passのclaimの前に自分のPATHで検査し（`Verifier::run_env_programs`）、見つからなければそのpassではclaimしない。
  走っているrun、review、triage、すでに始まったresumeのsessionは止めないが、`needs_session`のrunの新しいresumeはclaimと同じく始めない（resumeのwrapperにも`[run.env]`が渡るため）。
  queueの最新の`run_env_program_missing` / `run_env_program_found`と答えが変わったときだけ、見つからなくなったら`run_env_program_missing`、見つかるようになったら`run_env_program_found`を記録する。
  queueの記録と比べるので、supervisorを起動し直しても繰り返さない。
  `dagq.toml`が読めないときは前の答えのまま（provisioningがそのfileのerrorを出す）。
- **着地の保留**: 見つからない間、passしたrunは`awaiting_integration`のままleaseを持って着地slotを待ち（`Phase::AwaitingSlot`）、統合slotは取らない。
  見つかれば次のpassで着地に進む。
  検査はpassごとに（drain・handoff中も）行う。
  drain・claimの停止の最中に見つからなければ、supervisorは待たずにそのrunのleaseを返し（handoffは返さず次のprocessに渡す）、[拾い直し](supervise.md#leaseの解放)に任せる。
  `approve_landing`の`land`の答えは見つからない間もその場で適用してaskを閉じ、runを着地の列に並べる。
  列のrunの着地の開始（`start_approved_landings`）だけが、見つかるまで待つ（[Review](review.md#review-supervisor)の6）。
- **`integrate`の検証の前**: 検証コマンドを実行する前に同じ検査をrunの`run_dir`で行い、見つからなければ検証コマンドを実行せず、`dagq.toml`が読めないときと同じく着地処理のエラーにする（runは元の状態に戻り、`needs_session`にしないのでresumeを使わない。`last_error`に変数名と値とPATHが入る）。検証コマンドが無いtaskは検査しない。
- **変更の印**: supervisorは同じ周回で`[run.env]`を展開せずに読み（`Verifier::run_env_table`）、queueの秘密のsaltで鍵付けして正規化したhashがqueueの最新の`run_env_changed`と違うときだけ`run_env_changed`（hashと変わったキーの名前。値は書かない）を記録する。
  `RUSTC_WRAPPER = "sccache"`の追加・削除や`CARGO_BUILD_JOBS`の変更はこの印に表れる（[変更の印](commands.md)、[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定11）。
  attentionにはならない。
- **attention**: `run_env_program_missing`はinbox宛てのattentionで、`watch`はこのeventで起き、`status`は最新がmissingの間その行を出し、`run_env_program_found`で消える。
- **`doctor`**: `run_env`の欄に`doctor`を打ったプロセスのPATHでの検査の結果と、supervisorが最後に記録した答えを出し、`dagq.toml`が無く記録も無ければ欄ごと出さない。
- workerのwrapperのPATHは`[run.env]`で変わりうるので、supervisorの検査と同じとは限らない。workerのPATHでだけ見つからない場合は、workerの検証が失敗してreceiptかtriageで分かる。
- 2つのkindはtaskにもgoalにも紐づかない行として`run_events`に入る（[persistence](../persistence.md)）。


## 定期draft<a id="予定-定期draft未実装"></a>

[ADR-0072](../../adr/0072-recurring-drafts-from-dagq-toml.md)の決定1・2・5。
`[[recurring]]`は1表で1つの定期の提案で、欄の意味は`domain::recurring::RecurringDraft`、読み手と誤りの種類は`infrastructure::recurring_config`のdoc commentが持つ。
読むのは上の「main checkoutの決め方」の作業ファイルで、supervisorがruntimeのplannerを立てる各passで読み直す（`load_recurring`）。
表が無ければ登録しない。

- **誤り**: 表の誤りは`dagq.toml`全体の行番号付きの誤り。
  読めないpassは定期draftを登録せず、同じ誤りごとに1回logにwarnを出す。
  設定の誤りだけを人の判断のaskにはしない。
  `doctor`の`recurring`の`error`がその誤りを出す。
- **材料**: draftの材料は登録時のsnapshotで、設定の変更は次の登録から効く。
  keyの削除で既存のdraftをcancelせず、同じkeyの再追加や内容の編集で間隔と重複の防止をリセットしない。
  `goal`はそのgoalがtaskを受け付けるあいだだけdraftの所属にし、閉じていれば所属なしで登録して採用するplannerに任せる。
- **期限**: queueの同じkeyの最後の登録時刻（UTC）に`interval_days`の日数を足す。
  重複の防止、履歴の無いkey、停止中の期限の取り戻しは[ADR-0072](../../adr/0072-recurring-drafts-from-dagq-toml.md)の決定2のとおりで、登録時のtransactionとplannerへの材料は[recurringのdraft](draft-planners.md#recurringのdraft)。
- **見え方**: `status`と`stats`の`recurring`の`schedule`がkeyごとの最後の登録・終わっていないtask・次の登録の予定を出す（欄は`domain::recurring::RecurringSchedule`）。
