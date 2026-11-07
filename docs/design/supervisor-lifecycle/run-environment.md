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

- 書式と誤りの入口は`run_env::parse_config`。
  `DAGQ_`で始まるkeyと知らないkeyは拒む。
  旧バイナリは知らない表とkeyを含む`dagq.toml`全体を読めなくなり本番のqueueのrunがすべて止まるので、下の表とkeyは固定バイナリが読めるようになってから足す。
- 読むのはrepositoryのmain checkout（下の「main checkoutの決め方」）の作業ファイルで、run worktreeのものではない。
- worker・resume・review・検証への適用と読み込み失敗の扱いは`load_run_env`のdoc commentが持つ。
  読めなければ起動せず、resumeの試行も使わない（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。
  hostの検証の一時dirの境界は[Run worktrees](run-worktrees.md#終わっていないrunのビルド成果物)。
- `dagq.toml`はrepositoryにcommitされ、値はworkerのwrapperとturnのprocessのenvに渡るので、secretは入れない。
- この repositoryの`[run.env]`の値と理由、置かないものとsccacheの入れ方の規則は`dagq.toml`の`[run.env]`のコメント、`.config/nextest.toml`に並列度を書かない理由はその冒頭のコメント、その補足とほかのhostのツールの入れ方は[運用の開発文書](../../development/operations.md)の「`[run.env]`とtestの並列度の置き場」と「hostのツール」が持つ。
- cargo-nextestは`[run.env]`が名指すプログラムではないので下の検査の対象ではなく、無いhostでは着地の検証の失敗になる（[ADR-t1925-1](../../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)の決定8）。
- `[recheck]`は着地のたびにsupervisorが着地待ちのrunをmainに載せた木で実行する軽い検査で、keyの意味は`domain::recheck::RecheckConfig`、流れとcommandに渡るenvは`supervise::execution::recheck`が持つ（[ADR-0068](../../adr/0068-recheck-waiting-runs-after-each-landing.md)の決定2、[ADR-t2032-1](../../adr/2026-10-07-t2032-1-run-the-recheck-command-only-on-runs-touching-recheck-paths.md)）。

- reviewのagentの設定の書式は`run_env::REVIEW_SUBAGENTS_PREFIX`、built-inの意味は`domain::review_builtins`が持つ（[ADR-t1728-1](../../adr/2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)、[ADR-t2259-1](../../adr/2026-10-12-t2259-1-builtin-review-agents-and-programs-configured-by-builtin-names.md)）。
  起動の組と、landing branchから選ぶsnapshotの境界は[Review](review.md#reviewのsubagent)。

- `[disk]`はclaimと着地の検証の前に確かめる空き容量の閾値で、keyと既定は`domain::disk::DiskConfig`、読む時と読めないときの扱いは[claimを控える](claim-hold.md#空き容量)が持つ（ADR-0047の決定44）。

- `[resume]`は`needs_session`のrunの衝突だけの試行の上限で、keyと既定、設定にしない上限は`domain::resume::ResumeConfig`が持つ（[Needs session](needs-session.md)の「試行の数え方」、ADR-0047の決定24）。
  supervisorが起動時に読み（`SuperviseOptions::resume`で上書きできる）、読めなければ既定値で動く。

- `[retention]`は終わったplanner・plan review・observationのdirを残す日数`days`と`backups/`の数`backups`で、意味は`RetentionConfig`（ADR-t697-1）。

- `[supervisor]`はsupervisorの枠と控えの設定で、keyと意味は`domain::slot_limits::SupervisorConfig`が持つ（[人の答えを待つrun](waiting.md)・[Draft planners](draft-planners.md)・[Finding planners](finding-planners.md)・[Plan review (supervisor)](plan-review.md#plan-review-supervisor)・[claimを控える](claim-hold.md)・[実行区分の上限とaging](claim-hold.md#実行区分の上限とaging)、ADR-t1479-1、ADR-t1591-1）。
  projectごとのbuildの重さに合わせた並列数と、答えを待つplannerが枠をふさいでもdraftとreviseが進む数と、buildのloadが出るまでの間をrepositoryに置くため。
  値の優先順は`domain::slot_limits`、読み直しと変更の記録は`ClaimState::reread_limits`が持つ。
  各passの読み直しで着地した変更が効き、下げた枠を超える既存のrunやplannerを止めない。
  読めない間は使っている値を保つが、同じfileのほかの読み手の誤りはclaimと着地を止めうる。
  flagを付けて起動したsupervisorは引き継ぎ後もflagを優先する。
  fileに従わせる操作と再起動の呼び出し元は`domain::slot_limits`のdoc commentが持つ。
- `[provider_fallback]`は使えないproviderからもう一方への切り替えを止める設定で（[ADR-t1857-1](../../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)。`domain::provider_switch::ProviderFallback`）、`workers`と`jobs`を受け付ける。
  振る舞いと優先順位は[Provider lifecycle](../provider-lifecycle.md#使えないproviderからの切り替え)の「切り替えを止める設定」。
  読み手は`compose::supervisor`で、supervisorが起動時に読み（読めなければ既定で起動する）、各passで`[supervisor]`の後に読み直す（再起動なしで効く。ファイルが無いときは黙って、読めない・値が誤っているときはerrorごとに1回logにwarnを出して、使っている値のまま続ける）。
- `[fresh_session]`は`peak_context_above`（意味は`FreshSessionConfig`）だけを持つ（[ADR-t2080-1](../../adr/2026-10-08-t2080-1-fresh-session-on-send-back-and-resume-when-peak-context-exceeds-threshold.md)、振る舞いは[Provider lifecycle](../provider-lifecycle.md#大きなcontextからの新しいsession)）。
  keyが無ければ新しいsessionを始めない。
  値とkeyの誤り、読む時と読めないときの扱いは`[provider_fallback]`と同じ。
- `[replan]`は長期化したtaskの検知と再計画の診断の設定で、表が無いか`enabled = true`でなければ無効で何も検知・診断しない（keyと既定は`ReplanConfig`のdoc comment、流れは[taskの再計画](../../plans/task-replanning.md)）。
  supervisorが各passにmain checkoutから読む。
- `[eval]`と`[eval.providers.<provider>]`はagentのevalの上限・閾値・費用の見込みと採用した定義の見張りの有無（`watch`、既定true、次の回から効く）と時期で、keyと既定値は`EvalConfig`・`ProviderCost`のdoc comment、使い方は[agentのeval](../agent-eval.md)が持つ。
- `[throughput_review]`の`kpi_stats`（既定true）がfalseなら、スループットの見直しは`kpi`・`stats`を集めずjobに打たせない（意味は`ThroughputReviewConfig`、流れは[Throughput review](throughput-review.md#promptの入力)）。
  `dagq throughput-review`が各回main checkoutから読み、次の見直しから効く。
- `[review.jobs]`はrunのreviewの段のjobの種類ごとの時間の上限と、reviewのagentのjobの同時実行の上限で（ADR-t1895-1決定1。keyと既定値は`ReviewJobs`のdoc comment）。
  他のjobに効かず（evalのケースのprogramのjobはreviewの段に数える）、無いkeyは今の上限（[Headless job processes](headless-job-processes.md#記録)）。
  supervisorが起動時に読む。
- `[review.programs.<name>]`はreviewと着地の前のprogramのjobで（[プログラムのreview](review-programs.md)）、keyと意味は`ReviewProgram`が持つ。
  `[review.programs."builtin:<名前>"]`はbuilt-inの`enabled`と`paths`（既定を置き換える）だけを持ち（`BuiltinProgramSetting`）、ほかのkeyは誤り。
  読むのは作業ファイルでなく、landing branchのcommit（reviewは試行ごと、integrateはrebaseの先）の`dagq.toml`と`script`。
- `[review.code]`は上限の後の判定（[Review](review.md)の4a）のコードのpath、`[review] revise_limit`はrunを通した手直しの上限（ADR-t3033-1）で、意味と既定は`review_reason::CodePaths`・`ReviseLimit`が持つ。
  どちらも読むのはreviewの試行の始めのlanding branchのcommitの`dagq.toml`で、作業ファイルとrunのbranchは読まず、読めなければ既定に戻さず試行をreviewの失敗にする。
- `[tasks]`の分類とtitleの規則は`domain::ChangeSet`と`domain::TitlePrefixes`、関連文書のhintsは`domain::doc_hints`が持つ（[ADR-t980-1](../../adr/2026-09-29-t980-1-classify-runs-by-declared-change-and-diff-derived-area.md)、ADR-t3321-1）。
  runtimeが登録するdraftには、この検査を適用しない。
  planの検査手順は[taskの登録](../../development/task-registration.md#runtimeのdraftの検査)が持つ。
- `[goals]`のラベルの集合は`domain::TagSet`、Spikeの周期の上限の読み手は`compose::max_spike_cycles`が持つ（[ADR-t1639-1](../../adr/2026-10-04-t1639-1-goal-priority-is-the-source-tasks-inherit-and-goals-carry-tags.md)）。
  値は`dagq.toml`、各ラベルの意味と付け方は[taskの登録](../../development/task-registration.md#goal-の優先度とラベル)が持つ。
- `[e2e]`は`paths`だけを持ち、変えるとrunに`e2e`のevidenceを求めるpathをglobの1行の配列で書く（ADR-t963-1決定2）。
  読むのはsupervisorがvalidatingとworkerのpromptを作るたびのmain checkoutの作業ファイルで、読めなければ警告を出して空として扱う（本番を守るのは[Auto-update](auto-update.md)の入れ替え前のe2eの関門）。
  使い方と無いときの扱い、この repositoryの範囲は[Validation](validation.md)の8と「この repository で置く範囲」。
- `[local_checks]`と`[local_checks.gates.<name>]`は手元の関門の宣言で、reviewがmainのcommitから読む（ADR-t2589-1決定6）。
- `[areas]`は`stats`と`kpi`がrunを分けるareaの名前→globの1行の配列（ADR-t980-1。`domain::areas::AreaMap`）。
  書式は上の型、求め方は`application::areas::AreaReader`のdoc commentが持つ。

- `[kpi]`と`[kpi.targets."<KPI>"]`はKPIの判定の設定と目標で、KPIの計算（`kpi`・レポート・push・observerの入力）だけが読む（[計測のコマンド](commands.md)、[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定17・19。解析は`src/infrastructure/kpi_config.rs`の`KpiTables`）。
  `[kpi]`の設定の数値は既定のまま書かない。

- 互換のために受け付けるheadlessの欄は`run_env::HEADLESS_TABLE`が持つ（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)、[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)）。
  欄を消すのは、古い固定バイナリが欄の無い`dagq.toml`を読めることを確かめた後にする。
- `[worker.trial]`はworkerのmodel・effortの試し（群の割合と段の上の候補）の設定で、keyと既定は`domain::worker_model::WorkerTrial`が持つ（[Worker model](worker-model.md#限定の試しworkertrial)、ADR-t3957-1）。
  providerの組の試しの`[worker.provider_trial]`（keyと既定は`ProviderTrial`）も同じ節が持つ。
  taskに由来する失敗の後のworkerのsessionの段上げ（決定5、[Worker model](worker-model.md#段上げ決定5)）は設定を持たず、`[worker.trial]`の有無に関係なく働く。

- `[roles.<role>]`はworker以外のアクターのsessionのprovider・model・effortを持つ（[Actor model](actor-model.md)、ADR-0079の決定7、[ADR-t1063-1](../../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)の決定1）。
  役割・key・値と読み込みが拒むものは、`domain::actor_model`のdoc commentが持つ。
  読み込みの入口は`Verifier::role_models`で、読む時点と、読めないときと表の無いときの起動は[Actor model](actor-model.md)が持つ。
  runtimeのplannerの古い経路の欄は受け付けて無視し、runtimeのplannerは常に非対話で動く（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)の決定3）。
  その役割をCodexで動かす実装を持たないバイナリはCodexを拒むので、各節は固定バイナリがその役割の実装の着地commitを含むことを切り替えのverifyの関門で確かめてから足す。
- workerとintegrateのverificationを動かす場所の設定は、綴りと値を`RuntimeConfig`のdoc commentが、読み込みの入口と流れを[Container runtime](../container-runtime.md)が持つ。
- `[container.image]`はimageに足すprojectのContainerfileと入力を名指し、keyの綴りと意味は`ProjectImage`のdoc commentが、buildと読む場所は[Container runtime](../container-runtime.md#image)が、案は[profile](../toolchain-profiles.md#推奨の設定)が持つ。

- `[language]`はAIが人に向けて書く文の言語（`tag`、BCP 47の言語タグ）を持ち、利用者ごとの`config.toml`の同じ表より優先する（[Language](language.md)、[ADR-t616-2](../../adr/2026-09-27-t616-2-language-of-text-ai-writes-for-people-is-configurable.md)）。
  表は`run_env::parse_config`が中を見ずに受け付け、誤りでclaimや着地を止めないよう[Language](language.md)の読み手が検査する。

- `[repository]`は着地先のbranch・pushのremote・pushするかを持ち、欄と既定は`domain::landing_branch::RepositoryConfig`が持つ（[ADR-t615-1](../../adr/2026-09-27-t615-1-landing-branch-and-push-remote-per-repository.md)）。

- `[ci_watch]`はsupervisorが着地先のbranchのCIの結果を`gh`で読む見張りの設定で（[ADR-t1920-1](../../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)。振る舞いは[CI watch](ci-watch.md)）、表があるときだけ有効になる。
  keyの型・既定・意味は`domain::ci_watch::CiWatchConfig`（古さの`[[ci_watch.freshness]]`は`ci_freshness_config`）のdoc commentが持ち、読み直しと`up`のpreflightは[CI watch](ci-watch.md)の「いつ確かめるか」と「読む手段が無いとき」が持つ。
  この repositoryの値は`dagq.toml`の`[ci_watch]`が持ち、workflowとjobの名前や成果物（[CI watch](ci-watch.md)の「実行の扱い」と「定時のworkflow」）を変えたら同じ変更で合わせる。
- `[landing_verification]`は`integrate`がtaskの検証のコマンドの一部をrepositoryの着地の検証のコマンドに置き換える設定で（ADR-t1925-1。流れと境界は[Validation](validation.md#着地の検証)）、表があるときだけ働く。
  `integrate`が着地のたびにmain checkoutから読む（読めなければ`[run.env]`と同じく着地処理のエラー）。
  書式（key・型・既定値・検査）と、置き換えたコマンドにだけ`[run.env]`の後ろに足すenvの意味は`domain::landing_verification`のdoc commentが持つ。
  そのenvは`DAGQ_`で始まり、`[run.env]`は`DAGQ_`のkeyを拒むので衝突しない。
  何を置き換え、ITをどう選ぶかはrepositoryの設定とscriptが持ち、runtimeは知らない。
- `[verification]`の`command_timeout_secs`は検証のコマンド1本の時間の上限（秒）で、`integrate`・landing recheck・containerのverificationに掛かり、死んだ着地の解放の猶予も従う（範囲と既定は`domain::verification_limit`）。
- `[egress]`の`registries`は、containerのrunが[queueのbroker](../queue-broker.md)を通って届くregistryのhostを挙げる（[ADR-t2257-1](../../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定3）。
  書式・意味・既定は`run_env::EGRESS_REGISTRIES`のdoc commentが、案は[profile](../toolchain-profiles.md#推奨の設定)が持ち、brokerは`CONNECT`のたびにmain checkoutから読む。
- `[toolchains.<name>]`は使うtoolchainのprofileと当てるpathsの宣言で、宣言したprofileはpathsに依らず`integrate`とlanding recheckの検証の出力の読み方に効く（書式と誤りの扱いは`run_env::TOOLCHAINS_PREFIX`のdoc comment、当て方は[Toolchain profiles](../toolchain-profiles.md)）。
- `[verify_marks.<name>]`は検証の出力のprojectに固有の印で、既存の分類に印を足し、宣言したprofileに依らず`integrate`とlanding recheckに効く（書式は`run_env::VERIFY_MARKS_PREFIX`のdoc comment）。

## sccacheのserver

serverの所有とguardの境界は[sccacheの地図](sccache.md)。

### 外部serverと失敗の偏り

外部serverの診断と人への通知は[sccacheの地図](sccache.md#外部serverと失敗の偏り)。

## main checkoutの決め方

`dagq.toml`を読むmain checkoutは、repositoryのmain worktree（`git worktree list --porcelain`の最初のentry）で、決めるのはGitで`main_checkout_of`、Gitを起動しないqueue serviceでは束縛から`main_checkout_from_files`の2か所だけ（bareと`git init --separate-git-dir`などの構成の扱いはそれぞれのdoc comment）。
`dagq.toml`を読むコマンドとaskの通知のrepositoryの名前はどれもこの結果を使う。

main checkoutが無いとき、実行したcheckoutやrun worktreeの`dagq.toml`で代わりにしない（runごとに設定が変わりうるため）。
main checkoutが無いときの各コマンドの扱いは`main_checkout_of`、通知で使う名前は`naming_checkout`のdoc commentが持つ。

## `[run.env]`が名指すプログラムの検査

プログラムの解決の入口は`run_env::check_run_env_programs`（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定8・9）。
見つからない間はclaim・新しいresume・着地の開始を控え、失敗し続けるcargoで試行を使い切るのを防ぐ。
検査と遷移の記録は`Supervisor::check_run_env_programs`、着地待ちのleaseとslotの境界は`Phase::AwaitingSlot`が持つ。
upのpreflightは`lifecycle::Ports::run_env_programs`、integrateの検査は`Integrator::land`のdoc commentが持つ。
変更の印は値を書かず、queueの秘密のsaltで鍵付けしたhashを残す（`domain::marks`、[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定11）。
attentionとdoctorの表示の入口は`domain::run_env::RunEnvCheck`。
workerのPATHは環境の上書きでsupervisorと異なりうるので、workerでだけ見つからない問題は手元の検証で分かる。

## 定期draft<a id="予定-定期draft未実装"></a>

[ADR-0072](../../adr/0072-recurring-drafts-from-dagq-toml.md)の決定1・2・5。
欄と期限・材料の意味は`domain::recurring`、書式の検査は`infrastructure::recurring_config`、読み直しは`load_recurring`、登録のtransactionは`infrastructure::recurring_drafts`が持つ。
設定の誤りだけを人の判断のaskにしない。
登録は提案であり、採用は[recurringのdraft](draft-planners.md#recurringのdraft)のplannerが判断する。
