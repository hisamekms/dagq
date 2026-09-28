---
id: design-supervisor-lifecycle-run-environment
type: design
title: "Run environment"
status: current
created: 2026-09-26
updated: 2026-09-28
last_verified: 2026-09-28
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-landing-branch
  - design-supervisor-lifecycle-language
  - adr-0040
  - adr-0076
  - adr-0049
  - adr-0079
  - design-supervisor-lifecycle-worker-model
  - design-supervisor-lifecycle-actor-model
---

# Run environment

repository rootの`dagq.toml`の`[run.env]`（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定3。ADR-0040の決定3を引き継ぐ）が、runごとの環境変数になる。読み込みは`src/infrastructure/run_env.rs`の純粋関数（`parse_run_env`と`expand`、fileを読む`load_run_env`）で、ファイルが無ければ空。

- 書式はTOMLの部分集合: 表は`[run.env]`・`[stall]`・`[conflicts]`・`[recheck]`・`[disk]`・`[resume]`・`[exit]`・`[repository]`（[Landing branch](landing-branch.md)）・`[worker.trial]`・`[roles.<role>]`・`[language]`・`[supervisor]`・`[kpi]`（と`[kpi.targets."<KPI>"]`）だけを持ち（`[language]`はここでは表として受け付けるだけで中を見ず、[Language](language.md)の読み手が検査する。誤りでclaimや着地を止めないため）、`[run.env]`の各行は`KEY = 'literal'`か`KEY = "basic"`（`\\` `\"` `\n` `\t`のescape）。`#`以降はcomment。ほかの表、表の外のkey、環境変数名でないkey、重複したkey、`DAGQ_`で始まるkey（runtimeが`DAGQ_ROLE` / `DAGQ_QUEUE`に使う）はエラーにする。
- 値の`${DAGQ_QUEUE_DIR}`はqueue directory（DBのある directory）、`${DAGQ_RUN_DIR}`はそのrunのrun directoryに展開する。ほかの`$`は書いたまま残す（shellの展開はしない）。`${DAGQ_RUN_DIR}`はtask 91で加えた（ADR-0040の決定3、ADR-0049の決定3が引き継ぐ）。
- 読むのはrepositoryのmain checkout（下の「main checkoutの決め方」）の作業ファイルで、run worktreeのものではない。`integrate`をどのworktreeから呼んでも同じファイルを読む。検証コマンドが1件も無ければ読まない。
- 渡し先: (a) `provision`がworkerのworkspaceを作るとき、`DAGQ_ROLE` / `DAGQ_QUEUE`の後ろに`--env KEY=VALUE`で並べる（ADR-0026の仕組み）。worktreeを作る前に読むので、壊れた`dagq.toml`はprovisioningの失敗になり、workspaceは開かずsupervisorはclaimを止める。(b) `integrate`の`verification_commands`を`Command`のenvに足す（validatingは検証コマンドを実行しない）。読めないファイルは着地処理のエラーで、runは元の状態に戻る。(c) reviewのheadless実行（ADR-0049の決定2）のコマンドのenvに足す。(d) `needs_session`のresumeが開くworkspaceに、(a)と同じくworkerのenv（`DAGQ_ROLE` / `DAGQ_QUEUE`とworkerのactorの名前）の後ろに並べる（task 303）。resumeのsessionはworkerと同じ手元の検証（fmt・clippy・関係するtest・e2e）を回すので、workerと同じ`[run.env]`（sccache、buildとtestの並列度）が要るため。ADR-0049の決定3（ADR-0040の決定3を引き継ぐ）は渡し先をworkerのworkspace・`integrate`の検証・reviewの3つと書くが、resumeのsessionはrunのworkerのsessionの続きで、その「workerのworkspace」に含まれると読む。値は`begin_resume`の前に読み（`start_resume`に渡す）、landing branchと同じく読めなければ試行を使わずrunは`needs_session`のまま（landing branchの読み込みが同じfileを先に検査するので、壊れた`dagq.toml`はそこで止まる）。`[run.env]`が名指すプログラムが見つからない間は、claimと同じくresumeも始めない（下の「`[run.env]`が名指すプログラムの検査」。始めるとsessionのcargoがすべて失敗して試行を使うため）。resumeの時点のmain checkoutの`dagq.toml`を読むので、workerの起動の後に変えた値はresumeのsessionで効く。
- `dagq.toml`はrepositoryにcommitされ、値はworkspaceを開く`cmux`のargvに出るので、secretは入れない。
- この repositoryでは`dagq.toml`の`[run.env]`に`RUSTC_WRAPPER = "sccache"`と`SCCACHE_IGNORE_SERVER_IO_ERROR = "1"`を置き、依存crateのcompileの結果をsccacheでrun間に共有する（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定6・7、task 393）。sccacheは人が`mise use -g sccache`で入れ、`~/.local/bin/sccache`にmiseのshimへのlinkを置く。serverと話せないときはclientがrustcを直接実行する。targetは共有しない（ADR-0040の決定3をADR-0049の決定3が引き継ぐ。ADR-0023の決定3は`CARGO_TARGET_DIR = "${DAGQ_QUEUE_DIR}/target"`を置くとしたが、task 91の着地前のreviewの指摘を受けて2026-09-23にユーザーが決めた）。理由: (a) cargoのlockはbuildだけを直列化し、その後のtest実行は分離されないので、`CARGO_BIN_EXE_dagq`をexecするtest（`tests/it`の`cli_*`・`runtime_*`・`location`と、`plugin.rs`・`e2e.rs`）が、並行する別のrunのbuildが上書きした`target/debug/dagq`を実行しうる。(b) 同時の`cargo llvm-cov`が共有の`llvm-cov-target`のprofrawを消し合い・混ぜ合い、coverageの関門が誤る。`CARGO_TARGET_DIR`はrunごとの値でもsccacheの鍵に入って依存crateまで当たらなくなるので置かず、`CARGO_INCREMENTAL`も変えない（ADR-0049の決定6）。
- この repositoryでは`[run.env]`に`CARGO_BUILD_JOBS = "4"`も置き、runごとのcargoのbuildのjob数を絞る（2026-09-26に人がplannerと決めた。task 427）。渡し先(a)〜(d)のすべてに効くので、workerのworkspaceのcargo、`integrate`の`verification_commands`、reviewのheadless実行、needs_sessionのresumeが開くworkspaceのcargoが絞られる。理由: hostは8コア / 16GBで、並列4の運用でload averageが最大151〜204に達し、cmuxのcaptureのtimeoutが400件を超え、runのstartupの中央値が約1100秒になった（goal 36のnote 8718の基準値）。値の決め方: supervisorの`--parallel`を3に下げ、worker 3本と`integrate` 1本が同時にcargoを回しても合計16並列（コア数8の2倍）程度に収まるようにする。`--parallel`やhostを変えたら合わせて見直す。targetを共有しない理由(a)(b)は、envで並列度を絞ることには当たらない。
- testの並列度はbuildと分けて考え、`[run.env]`に`RUST_TEST_THREADS = "8"`（1つのtest binaryの中のtestのthread数）を置く（task 427ではbuildと合わせて4に絞っていたが、2026-09-26に人がplannerと8に上げると決めた。task 566）。理由: testは待ちが中心でCPUをあまり使わない（task 537の測定でtestのprocess 4本のCPUは0.1〜0.4コア、coverageの関門のtest段はper-testの時間の合計 ÷ 並列数で律速する。[nextest の測定](../../plans/nextest-measurement.md)）ので、並列度を上げてもhostの負荷は小さく、`integrate`のtest段が縮む。着地後の本番のload・cmuxのcaptureのtimeout・時間の上限を持つtestの失敗はplannerがstatsで前後を見て、悪化すれば6か4に戻す。
- 同じ考え方で`[run.env]`に`NEXTEST_TEST_THREADS = "8"`を置き、coverageの関門（`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`）でcargo-nextestが同時に走らせるtestのprocess数を決める（[ADR-0076](../../adr/0076-run-the-coverage-gate-tests-with-nextest.md)の決定2、task 518で4、task 566で8）。nextestは`RUST_TEST_THREADS`を読まず、既定ではCPU数だけ走らせるため。`.config/nextest.toml`には並列度を書かず（CIと人の手元のnextestまで絞られるため）、60秒を超えたtestを`SLOW`と出す`slow-timeout`だけを置く。cargo-nextestはsccacheと同じく人が`mise use -g cargo:cargo-nextest`で入れて`~/.local/bin/cargo-nextest`にmiseのshimへのlinkを置くが、`[run.env]`が名指すプログラムではないので決定9の検査の対象ではなく、無いhostでは`integrate`の検証の失敗になる（ADR-0076の決定3）。
- `[recheck]`は`command`（文字列。空白だけは拒否）の1 keyだけを持ち、着地のたびにsupervisorが着地待ちのrunをmainに載せた木で実行する軽い検査になる（[Landing recheck](landing-recheck.md)、[ADR-0068](../../adr/0068-recheck-waiting-runs-after-each-landing.md)の決定2。`load_recheck_command`、`Verifier::recheck_command`）。表が無ければmerge-treeだけを見る。commandには`[run.env]`と、queue dirの`recheck/target`を指す`CARGO_TARGET_DIR`が渡る。この repositoryでは固定バイナリを`[recheck]`を知るものに入れ替えた後に`command = "cargo check --locked --all-targets"`を足す（旧バイナリは未知の表を拒むので、先に足すとqueue全体が止まる）。

- `[disk]`はclaimと着地の検証の前に確かめる空き容量の閾値で、`sample_runs`（正の整数、既定20）、`claim_factor`（正の数、既定2）、`integrate_factor`（正の数、既定1.5）、`min_free_bytes`（正の整数、既定なし）を持つ（[空き容量を確かめる](disk-space.md)、ADR-0047の決定44、task 377。`load_disk_config`）。supervisorが起動時に読み、読めなければ既定値で動く。`[recheck]`と同じく、旧バイナリは未知の表を拒むので、固定バイナリを`[disk]`を知るものに入れ替えてから足す

- `[resume]`は`needs_session`のrunの衝突だけの試行の上限で、`conflict_only_limit`（正の整数、既定5）の1 keyだけを持つ（[Needs session](needs-session.md)の「試行の数え方」、ADR-0047の決定24、task 512。`load_resume_config`、`domain::resume::ResumeConfig`）。未知のkey・0以下・整数でない値・重複は他の表と同じく行番号付きのエラーにする。数える試行の上限`MAX_RESUME_ATTEMPTS`（3）は設定にしない。supervisorが起動時に読み（`SuperviseOptions::resume`で上書きできる）、読めなければ既定値で動く。起動の後の変更はsupervisorを起動し直すまで効かない。`[recheck]`と同じく、旧バイナリは未知の表を拒むので、固定バイナリを`[resume]`を知るものに入れ替えてから足す
- `[exit]`はsessionが止めた`/exit`の再試行（[Receipt and session exit](receipt-and-session-exit.md#exitの再試行)、ADR-0047の決定25、task 555。`load_exit_config`、`domain::exit::ExitConfig`）で、`retries`（0以上の整数、既定3。0は再試行と、使い切った後にworkspaceを閉じて着地へ進める経路を行わない）と`retry_intervals_secs`（正の整数の秒の配列`[30, 60, 120]`の形、既定`[30, 60, 120]`。n回目の再試行の後に待つ秒で、回数より短ければ最後の値を繰り返す）の2 keyを持つ。未知のkey・負の数・整数でない値・空の配列・配列でない値・重複は行番号付きのエラーにする。supervisorが起動時に読み（`SuperviseOptions::exit`で上書きできる）、読めなければ既定値で動く。起動の後の変更はsupervisorを起動し直すまで効かない。旧バイナリは未知の表を拒むので、固定バイナリを`[exit]`を知るものに入れ替えてから足す

- `[supervisor]`はsupervisorの同時に走らせるrunの数`parallel`（1以上の整数）と、人の答えを待つrunの上限`max_waiting`（0以上の整数。0で待ちを使わない。[人の答えを待つrun](waiting.md)）と、runtimeが同時に立てるplannerの上限`runtime_planners`（1以上の整数。draft・finding・reviseのplannerで共有し、run slotとは別、人が開いたplannerは数えない。[Draft planners](draft-planners.md)・[Finding planners](finding-planners.md)・[Plan review (supervisor)](plan-review.md#plan-review-supervisor)）を持つ（task 698、`runtime_planners`はtask 941。`load_supervisor_config`、`domain::slot_limits::SupervisorConfig`）。projectごとのbuildの重さに合わせた並列数と、答えを待つplannerが枠をふさいでもdraftとreviseが進む数をrepositoryに置くため。未知のkey・範囲外や整数でない値（`parallel`と`runtime_planners`は65535まで）・重複は他の表と同じく行番号付きのエラーにする。
  - **優先順**: どのkeyも、`supervise`のflag（`--parallel` / `--max-waiting` / `--runtime-planners`）の明示 > `[supervisor]` > 既定（`parallel`と`max_waiting`は4、`runtime_planners`は1。`SlotLimits::resolve`）。`up`はflagを明示されたときだけ`supervise`の引数に渡し、明示が無ければ焼き込まない（[`up` / `down`](up-down.md)）ので、`up`で起動したsupervisorもこの順で決まる。
  - **効くとき**: supervisorが起動時（引き継ぎのexecの後も）に読み、続けてループの各passで`[conflicts]`の読み直しの後に読み直す（[`supervise`](supervise.md)）。flagで3つとも明示したsupervisorは読まない。読み直した値が使っている値と違えば、そのpassから新しいclaimと待ちとplannerを立てる判定に使い、登録の`parallel` / `max_waiting` / `runtime_planners`と出どころを書き換え、queue event `supervisor_config_changed`（`from`・`to`（どちらも`parallel`・`parallel_source`・`max_waiting`・`max_waiting_source`・`runtime_planners`・`runtime_planners_source`）・`supervisor`）を記録する。main checkoutに着地した変更は`up`し直さずに効く。下げた値を超えて走っているrunと待っているrunと開いているruntimeのplannerはそのまま続き、下回るまで新しいclaimと待ちとplannerが控えられる。表を消すと既定（`default`）に戻る。ファイルが無いか読めない・値が誤っているときは使っている値のまま続ける（起動時に読めなければ既定値で起動する。どちらもlogにwarnを出す）。ただし誤った`dagq.toml`は`[repository]`などの読み手も止めるので、claimと着地も止まる。
  - **見え方**: `status`の`supervisors[]`の`parallel_source` / `max_waiting` / `max_waiting_source` / `runtime_planners` / `runtime_planners_source`と、`slots.source` / `waiting.source`（`flag` / `dagq.toml` / `default`。出どころの列より古いbinaryの登録はnull。`runtime_planners`は値も記録されないのでnull）。`doctor`の既定の出力も`parallel`・`parallel_source`・`max_waiting`・`max_waiting_source`・`runtime_planners`・`runtime_planners_source`を出す。
  - 起動し直すときの引き継ぎ: `install --allow-breaking`のdrainと自動更新の見張りがsupervisorを`up`で起動し直すときは、止めた登録の値のうち出どころが`flag`のもの（と、出どころの列より古いbinaryの登録の値）だけを`--parallel` / `--max-waiting` / `--runtime-planners`で渡し、`dagq.toml`か既定から決めた値は渡さない（`SupervisorRegistration::flag_arguments`。`runtime_planners`は値を記録しない古いbinaryの登録からは渡さない）。引き継ぎのexecは元のargvのまま続くので、この変更より前の`up`が`--parallel N`を付けて起動したsupervisorは、入れ替わっても`flag`のままで`[supervisor]`を読まない。`[supervisor]`に従わせるには、`down --wait`の後にflagなしの`up`で起動し直す。
  - `[recheck]`と同じく、旧バイナリは未知の表を拒むので、固定バイナリを`[supervisor]`を知るものに入れ替えてから足す。この repositoryの`dagq.toml`にはまだ置かない（`--parallel 3`はAGENTS.mdの`up`のコマンドが渡す）。

- `[kpi]`と`[kpi.targets."<KPI>"]`はKPIの判定の設定と目標で、`dagq kpi`だけが読む（[kpi](kpi.md#目標targets)、[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定17・19。`load_kpi_settings`、解析は`src/infrastructure/kpi_config.rs`の`KpiTables`）。`[recheck]`と同じく、旧バイナリは未知の表を拒むので、固定バイナリを`[kpi]`を知るものに入れ替えてから足す（先に足すとqueue全体が止まる）。この repositoryの`dagq.toml`には、固定バイナリがtask 430を含んでから、人とplannerがask 112で決めた`all`の層の7つの目標（`first_pass_rate`・`revise_rate`・`conflict_rate`・`verification_failed_rate`・`failed_rate`・`resumes_per_run`・`asks_per_landing`）を足した（task 572。値と経緯は`dagq.toml`のcomment）。`[kpi]`の設定の数値は既定のまま書かない。

- `[worker.trial]`はworkerのmodelの限定の試しの設定で、`enabled`（`true` / `false`、既定`false`）と`window`（正の整数、既定60。対象の判定で比べる直近の予測の件数）を持つ（[Worker model](worker-model.md)、[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)の決定4。`load_worker_trial`、`Verifier::worker_trial`）。supervisorがclaimのpassごとに読み、読めなければ試しの外でclaimする。既定は無効で、有効にするのは人の判断。この repositoryの`dagq.toml`には置かない。旧バイナリは表ごと拒むので、足すのはそれを知るバイナリに入れ替えた後にする。taskに由来する失敗の後のworkerのsessionの段上げ（決定5、[Worker model](worker-model.md#段上げ決定5)）は設定を持たず、`[worker.trial]`の有無に関係なく働く。

- `[roles.<role>]`はworker以外のアクターのsessionのmodelとeffortで、`<role>`は`plan_review` / `review` / `recovery` / `observer` / `runtime_planner` / `planner`、keyは`model`（空でない文字列）と`effort`（`low` / `medium` / `high` / `xhigh` / `max`）のどちらか・両方（[Actor model](actor-model.md)、[ADR-0079](../../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)の決定7。`load_role_models`、`Verifier::role_models`）。知らないrole・key・effort、空のmodel、重複はエラーにする。keyの無い表と表の無い役割は今までと同じ起動（`--model` / `--effort`を渡さない）。supervisorとobserverはjobやplannerを起動するたびに読み、読めなければ今までと同じ起動にする。`dagq plan`は読めなければ`warnings`に足して今までと同じ起動にする。この repositoryの`dagq.toml`には置かない。旧バイナリは表ごと拒むので、足すのはそれを知るバイナリに入れ替えた後にする。

- `[language]`はAIが人に向けて書く文の言語（`tag`、BCP 47の言語タグ、既定は無しで指示しない）を持ち、利用者ごとの`config.toml`の同じ表より優先する（[Language](language.md)、[ADR-t616-2](../../adr/2026-09-27-t616-2-language-of-text-ai-writes-for-people-is-configurable.md)）。旧バイナリは表ごと拒むので、足すのはそれを知るバイナリに入れ替えた後にする。

- `[repository]`は着地先のbranch（`branch`、既定は推定）、pushのremote（`remote`、既定`"origin"`）、pushするか（`push`、既定`true`）を持つ（[Landing branch](landing-branch.md)、[ADR-t615-1](../../adr/2026-09-27-t615-1-landing-branch-and-push-remote-per-repository.md)）。3つのkeyはどれも読み（`load_repository_config`）、ほかのkeyは未知のkeyとして拒む。`[repository]`を知らない旧バイナリは表ごと拒むので、足すのはそれを知るバイナリに入れ替えた後にする。dagq自身のrepositoryは既定で今までどおり`main`と`origin`になるので足さない。

## main checkoutの決め方

`dagq.toml`を読むmain checkoutは、repositoryのmain worktree（`git worktree list --porcelain`の最初のentry）で、決めるのは`src/infrastructure/adapters.rs`の`main_checkout_of`の1か所だけ（task 669）。`GitRepository::inspect`（`checkout()`）、`supervise`・`integrate`・`plan`・`up`（`RepositoryPaths.checkout`。Claude Codeのtrustの確認も同じpathで引く）、queueの束縛から引く`doctor`・`stats`・`observer`など（`bound_checkout`）、`--from`なしの`install`の既定のcheckout、askの通知のrepositoryの名前（`naming_checkout`）が、どれもこの結果を使う。Git common directoryの名前（`.git`かどうか）では決めない。

| 構成 | main checkout |
| --- | --- |
| 通常（common directoryが`<checkout>/.git`） | その`<checkout>`。どのworktreeからでも、common directory自体からでも同じ |
| bare（main worktreeに作業ファイルが無い。bareのrepositoryに`git worktree add`した構成を含む） | 無い。理由（`... is bare ...`）とbareでないcloneを使う案内のerror |
| `git init --separate-git-dir`（common directoryがmain worktreeの外） | Gitはmain worktreeの場所を記録せず、`worktree list`はcommon directoryそのものを出す。実行したcheckoutがmain worktree（`--absolute-git-dir`がcommon directory）ならそれ、linked worktreeやcommon directoryからは分からず、main worktreeで打つよう案内するerror（ask 142で人が決めた） |

main checkoutが無いとき、実行したcheckoutやrun worktreeの`dagq.toml`で代わりにしない（runごとに設定が変わりうるため）。`up`はpreflightで`...; the supervisor was not started`で止まり、`supervise`と`integrate`は起動時に同じerrorで止まり、`doctor`の`repository`は`error`を出して`run_env`は出さない。`plan`は`[roles.planner]`を読めない警告を出して既定で開き、`--from`なしの`install`は`--from`を求めるerrorになる。askの通知は、main checkoutが無ければqueueが束縛されたcommon directoryの名前で出す（束縛の無いqueueでは実行したcheckout）。supervisorのaskの通知は`Layout.main_checkout`の名前で出す。

## `[run.env]`が名指すプログラムの検査

[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定8・9（task 395）。`[run.env]`の`RUSTC_WRAPPER`などが実行できないと、runのcargoは`could not execute process`ですぐ失敗し、そのまま流すとworkerの検証も`integrate`の検証も全部落ちてresumeを使い切る。そこでruntimeが、cargoを走らせる前に解決できるかを見る。

- **検査する変数**: cargoがプログラムとして実行する`RUSTC_WRAPPER`・`RUSTC_WORKSPACE_WRAPPER`・`RUSTC`・`RUSTDOC`と、その`CARGO_BUILD_`付きの形の8つ（`src/infrastructure/run_env.rs`の`PROGRAM_VARIABLES`）。それ以外の変数は見ない。
- **解決の規則**（`resolve_program`・`check_programs`・`check_run_env_programs`）: 値が空なら検査しない。`/`を含む値はそのpathが実行可能なfile、含まない値は検査するプロセスのPATHの各directoryに実行可能なfileがあること。`${DAGQ_QUEUE_DIR}` / `${DAGQ_RUN_DIR}`は展開してから見る。runの無い検査（`up`・claim・`doctor`）では`${DAGQ_RUN_DIR}`を含む値は検査しない（runの前にはrun directoryに何も無いため）。`dagq.toml`が無ければ何も検査せず、今までどおりに動く。結果は`domain::run_env::RunEnvCheck`（`config`、`path`、`programs[]`の`variable` / `value` / `resolved`）。
- **`up`のpreflight**: cmux・Claude・trustの後に、`up`のPATH（supervisorに渡す`UpEnvironment.path`）で検査する（`lifecycle::Ports::run_env_programs`）。見つからなければsupervisorを起動せず、変数名・値・PATHと対処（入れるか、`dagq.toml`から外すtaskを登録する）を挙げたerrorで止まる。`dagq.toml`があるrepositoryでは`up`の出力に`run_env`（検査の結果）が付く。
- **supervisorのclaimの前**: 各fill passのclaimの前に自分のPATHで検査する（`Verifier::run_env_programs`）。見つからなければそのpassではclaimしない。走っているrun、review、triage、すでに始まったresumeのsessionは止めないが、`needs_session`のrunの新しいresumeはclaimと同じく始めない（resumeのworkspaceにも`[run.env]`が渡るため。task 303）。queueの最新の`run_env_program_missing` / `run_env_program_found`と答えが変わったときだけ記録する（見つからなくなったら`run_env_program_missing`、payloadは最初に見つからなかった`variable` / `value`と`path`、全部の`programs`、`message`、`supervisor`。見つかるようになったら`run_env_program_found`）。queueの記録と比べるので、supervisorを起動し直しても繰り返さない。`dagq.toml`が読めないときは前の答えのまま（provisioningがそのfileのerrorを出す）。
- **着地の保留**: 見つからない間、passしたrunは`awaiting_integration`のままleaseを持って着地slotを待ち（`Phase::AwaitingSlot`）、統合slotは取らない。見つかれば次のpassで着地に進む。検査はpassごとに（drain・handoff中も）行う。drain・handoff・claimの停止の最中に見つからなければ、supervisorは待たずにそのrunのleaseを返し、runは`awaiting_integration`のまま人の`review and integrate`になる。`approve_landing`の`land`の答えも、見つかるまで適用しない。
- **`integrate`の検証の前**: 検証コマンドを実行する前に同じ検査をrunの`run_dir`で行い、見つからなければ検証コマンドを実行せず、`dagq.toml`が読めないときと同じく着地処理のエラーにする（runは元の状態に戻り、`needs_session`にしないのでresumeを使わない。`last_error`に変数名と値とPATHが入る）。検証コマンドが無いtaskは検査しない。
- **変更の印**: supervisorは同じ周回で`[run.env]`を展開せずに読み（`Verifier::run_env_table`）、queueの秘密のsaltで鍵付けして正規化したhashがqueueの最新の`run_env_changed`と違うときだけ`run_env_changed`（hashと変わったキーの名前。値は書かない）を記録する。`RUSTC_WRAPPER = "sccache"`の追加・削除や`CARGO_BUILD_JOBS`の変更はこの印に表れる（[変更の印](marks.md)、[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定11）。attentionにはならない。
- **attention**: `run_env_program_missing`はinbox宛てのattention（`next: install tool`）で、`watch`はこのeventで起き、`status`は最新がmissingの間`kind: run_env_program_missing`、`status: missing`、`last_error`にその`message`を出す（`run_id` / `task_id`はnull）。`run_env_program_found`で消える。
- **`doctor`**: `run_env`の欄に`config`、`doctor`を打ったプロセスのPATH（`path`）、`programs`（`resolved`は見つからなければnull）、`missing`（件数）、supervisorが最後に記録した`run_env_program_missing` / `run_env_program_found`（`supervisor_last`）を出す。`dagq.toml`が無く記録も無ければ欄ごと出さない。
- workerのworkspaceのPATHはsupervisorからは見えない。workerのPATHでだけ見つからない場合は、workerの検証が失敗してreceiptかtriageで分かる。
- 2つのkindはtaskにもgoalにも紐づかない行なので、`0032_run_env_program_events.sql`が`run_events`を作り直してCHECKに足した（[persistence](../persistence.md)）。

