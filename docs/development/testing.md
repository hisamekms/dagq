---
id: development-testing
type: development
title: このrepositoryのtestの制約（coverageと着地の検証・test binary・置き場所・書き方・macOSに固有のtest・判断と境界のtest・ファイルの行数・待ちの上限・e2eとその印・手動スモーク）
status: current
created: 2026-10-03
owners:
  - hisamekms
tags:
  - testing
  - conventions
related:
  - adr-t1453-2
  - adr-t1582-1
  - adr-t1410-1
  - adr-t1707-1
  - design-slow-tests
  - development-local-checks
  - development-task-registration
  - development-migrations
  - design-linux-ci
  - plan-local-checks-history
---

# このrepositoryのtestの制約

testを書く・置く・直すときの今の規則。読むのは、`tests/`・`src/`の`#[cfg(test)]`・`.config/e2e-quarantine.toml`を変えるworkerと、それらを変えるtaskを登録するplanner。手元でどのtestを流すかは[手元の検証](local-checks.md)、taskのverifyの選び方は[taskの登録](task-registration.md)、migrationの規則は[migration](migrations.md)、経緯は[手元の検証とtestの規則の経緯](../plans/local-checks-history.md)とADRが持つ。

## coverage

- 行カバレッジの合計を80%以上に保つ（`cargo-llvm-cov`、行基準、全体）。80%を見る関門はCIだけで、CIが最終関門になる（[ADR-t1925-1](../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)決定1・5）。下回る変更も着地し、CIの見張りが修正taskにする（[CI watch](../design/supervisor-lifecycle/ci-watch.md)）。
- CIの関門のコマンドは`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`。cargo-nextestがtestを1件ずつ別processで、binaryをまたいで並列に流す（ADR-t1925-1決定8）。dagqは`[workspace]`の無い単一のpackageで、`--workspace`はdagqだけを指し、付けない形と同じ範囲を測る（[ADR-t2126-1](../adr/2026-10-09-t2126-1-single-package-without-workspace-and-coverage-gate-keeps-workspace-flag.md)）。
- 着地の関門は、`integrate`がrebase後にworkerのreceiptを信用せず1回だけ流すtaskの`verification_commands`で（validatingでは実行しない）、そのうちcoverageの関門の行は`dagq.toml`の`[landing_verification]`がunit test全件と影響範囲で絞ったITに置き換える（下の「test binary」）。workerが手元で流すものは[手元の検証](local-checks.md)、taskのverifyの書き方は[taskの登録](task-registration.md)の「coverageの関門」。

## test binary

testは`src/lib.rs`のunit testと`tests/it`・`tests/e2e.rs`・`tests/plugin.rs`にある。このrepositoryのcrateにdoctestは無く、nextestはdoctestを流さない。

- 着地で流れるもの: `sh scripts/landing-it.sh`が1回の`cargo nextest run`で、unit test全件と、CIの夜間の対応表と差分で選んだIT（`tests/e2e.rs`を除く。差分で足した・変えたtestと表に無いtestを必ず含み、絞ったITの見込みの時間が上限を超えるとき・共通のファイルや表が判断できないファイルに触れたとき・表が取れないか古すぎるときはITを全部）を流し、1件でも落ちれば失敗する。既に落ちているtest（[CI watch](../design/supervisor-lifecycle/ci-watch.md)の一覧）は外し、CIの修正taskのrunでは直す対象のtestを外さない。選び方と値はscriptの冒頭、根拠は[着地のITの絞り込みの測定](../plans/landing-it-selection.md)の「決めたこと」。e2eは別に、要るrunで着地の前にruntimeがhostで流す（[Validation](../design/supervisor-lifecycle/validation.md)の「runtimeが流すe2e」）。
- CIで流れるもの: coverageの関門（上の「coverage」）が全てのtest binaryを全部実行し、1件でも落ちるか80%を下回れば失敗する。着地の検証が見逃した壊れはここで見つかる。

## testの置き場所

- e2e（`tests/e2e.rs`）とplugin（`tests/plugin.rs`）以外のintegration testは1つのtest binary `it`（`tests/it/main.rs`）にまとめ、testは機能ごとのファイル`tests/it/<機能>.rs`に足す。新しいファイルは`tests/it/main.rs`に`mod <機能>;`を足す。`Cargo.toml`は`autotests = false`で`[[test]]`は`it`・`e2e`・`plugin`の3本（[ADR-0078](../adr/0078-one-integration-test-binary.md)）。
- 複数のファイルで使うhelperは`tests/common`（`it`と`plugin`の両方が読む。`tests/it`からは`crate::common`）、runtimeのtestだけが使うfixtureとhelperは`tests/it/runtime_support`（`crate::runtime_support`）に置く。

## testの書き方

- 関門ではtestは1件ずつ別processになるので、binaryの中の他のtestとprocessの状態（staticや一度だけの初期化）を共有することに頼るtestを書かない。
- `cargo test`では同じprocessのthreadで走るので、process全体の状態（env・cwd）を変えるtestを書かない。
- testのshellに渡る文字列（stubのagent・reviewerのscript、taskの`verification_commands`、wrapperのscriptなど）にfilesystemのpathを埋めるときは、`'{}'`と単引用符で直接囲まず、`tests/common`の`shell_path`（`dagq::infrastructure::adapters::shell_quote`を包む）で引用する。runtimeのtestのfixtureはqueueとrepositoryを`queue's data.db`・`repo's directory`とapostropheを含む名前で作るので、その下のpathを単引用符で直接埋めると引用が途中で閉じてshellがsyntax errorで終わり、testが意図を確かめないまま通るか不安定に落ちる。SQL・TOMLの文字列リテラルとpathでない文字列は対象外。

## macOSに固有のtest

testはmacOS（関門とCIの`checks`）とLinux（CIの`linux`、失敗を通さない）の両方で通す（goal 83、[Linux build and test job in CI](../design/linux-ci.md)）。

- まず移植できる形に書く: `ps`・`lsof`・`sed`の出力と引数の違い、`/private/tmp`などのpath、inodeの再利用、時刻の精度（Linuxの`ps`のCPU時間は秒単位、Gitは秒単位で比べることがある）に頼らない。stat dataを変えるならmtimeを秒単位で動かす（`File::set_modified`）。
- macOSにしか無い機能（launchd・実物のcmuxなど）を使うtestだけを`#[cfg(target_os = "macos")]`で分け、直前のコメントに理由（何がmacOSにしか無いか）を書き、[Linux build and test job in CI](../design/linux-ci.md)の「macOSに固有として分けたtest」に足す。Linuxで消すのはtestだけで、runtimeの機能は消さない（Linuxで使えない機能は分かるerrorにし、それを`#[cfg(not(target_os = "macos"))]`のtestで確かめる）。
- worker（macOS）はLinuxのtestを流せないので、Linuxの結果は着地後のmainのCIの`linux`のjobのsummaryで読む。

## 判断と境界のtest

[ADR-t1410-1](../adr/2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)。runtimeのtaskのworkerとplannerが守る。

- 状態の判断（状態の遷移・回数と上限・時刻を値で受けた時間の判定・verdictやanswerから操作への対応・askやerrorの文面・次の一手の選び方）は`src/`の副作用のない関数にして`#[cfg(test)]`のunit testで確かめる。unit testは外部プロセス・git・SQLiteのファイル・sleep・実時間の時計を使わない（時刻は値で渡す）。
- `tests/it`はSQLite・Git・プロセス・supervisorの配線・復旧とadopt・cmuxの境界を代表の1 caseで確かめ、判断のcaseごとにfixtureとsupervisorを起動し直さない。
- e2eは実バイナリ・実Git・実cmuxのハッピーパスと境界だけにする（流し方は下の「e2e」）。
- integration testを減らすときは、確かめていた中身をunit testか残すintegration testに対応づけ、行き先の無いまま消さない。
- 時間の関門（[ADR-t1707-1](../adr/2026-10-05-t1707-1-time-gate-for-added-or-changed-integration-tests.md)）: baseからの差分で足した・本文を変えた`tests/it`のtestの1本の時間が閾値を超えたら、許可の一覧に項目が無いかぎり関門が落ちる。閾値・許可の一覧の置き場所と書式・testの名前の求め方・scriptの引数は[Slow tests](../design/slow-tests.md)の「itのtestの時間の関門」が持つ。
  - 超えたら、まずtestを直す（判断をunit testへ移す、caseごとにfixtureとsupervisorを起動し直さない）。直さずに許可の一覧に項目を足してよいのは、理由が次のどちらかのときだけ: (a) そのtestが守る境界（SQLite・Git・プロセス・supervisorの配線・復旧とadoptのどれか。cmuxは挙げず、inboxへのcmuxの送り出しはsupervisorの配線として書く）を書ける、(b) 移し替えの予定があり、その行き先のtaskかgoalを書ける。判断のcaseを並べただけのtestは(a)に当たらない。項目には足したtaskのIDを書く。
  - 項目のtestを直すか移すtaskは、同じ変更で自分の項目を外すか理由を直す。
  - 関門の場所: workerの手元（[手元の検証](local-checks.md)の「itのtestの時間の関門」）、runのreviewのtestの規則の検査、CIのpushとpull_request（mainへのpushで落ちればci-failureのissue）。`integrate`の検証には足さない。
  - helperやfixtureだけの変更は対象にならず（それを使うtestが遅くなっても捕まえない）、CIのrunnerと本番の関門では秒が違う。

## testファイルの行数

- `tests/*.rs`・`tests/common/*.rs`・`tests/it/**/*.rs`（tests/の下の全`.rs`）はどれも3,000行以下に保つ（1つのbinaryにまとめてもファイル単位のまま）。末尾にtestを足す形の大きなファイルは別々のtaskの追記が同じ場所で衝突するため。
- `scripts/check-test-file-lines.sh`が超えたファイルを名前と行数つきで出してexit 1にし、CIも実行する。
- 超えそうなら、testを足す前に機能ごとのファイル（`tests/it/runtime_*.rs`・`tests/it/lifecycle_*.rs`のように）へ分け、複数のファイルで使うhelperは`tests/common`に、runtime系だけのものは`tests/it/runtime_support`に寄せる。
- `src/`はまだ対象外。

## 待ちの上限

- testの待ちには上限を付ける。pollのloopはdeadlineを持ち、上限の無い待ち（threadのjoin、stubのsessionの終了、`dagq`の子プロセスの`output()`）は`tests/common/mod.rs`の`within`（fixtureが持つtest全体の`common::test()`と、1つの待ちの`STEP_LIMIT`）で包む。
- 上限を過ぎるとtest binaryがtestの名前と待っていた条件をstderrに出してexit 101で失敗する（`cargo test --test it`では同じbinaryの残りのtestもそこで止まる。関門のnextestはtestごとのprocessなので止まるのはそのtestだけ）。`cargo test | tail`が戻らなくなることはない。
- testが起動するシェルの待ち・ループ・stub（taskのverification、stubのagentやturnのscript、reviewerやjobのscript、wrapperのscript、`#[cfg(test)]`の子プロセス）は、testが成功・失敗・panic・時間切れのどれで終わっても終わる形にする。testが書くファイルを待つときは`while [ ! -f ... ]`を直接書かず、`tests/common`の`await_file`（シェルの単語を待つ）か`await_path`（pathを引用して待つ）を使う。`headless_claude`と`headless_codex`のstub（scriptの前置きの`TURN_PRELUDE`・`RESUME_TURN_PRELUDE`をその中で読む）は同じシェル関数`await_file`を定義しているので、その中では`await_file "$EXIT.go"`と書く。この待ちは、待つファイルのディレクトリ（testの`TempDir`）が無くなるか、testのprocess（headlessのstubでは`STUB_TEST_PID`、それ以外はシェルの親）が居なくなると抜ける。
- 無限ループ（`while :`）や長い`sleep`を持つ子は、親を失ったら（`kill -0 "$PPID"`が失敗したら）終わる条件か、`Drop`で子かそのprocess groupをkillするguardの下に置く。`Drop`のguardだけでは足りない: 時間切れの`process::exit`は`Drop`を通らないので、同じkillを`tests/common`の`on_timeout`に登録する（`KillOnDrop`と`runtime_support`の`Fixture`の形）か、子に親の死で終わる条件を持たせて組にする。headlessのstubは`HEADLESS_WATCHDOG`が、testのprocess（`STUB_TEST_PID`）かstubのディレクトリが無くなったら、`StubSpawner`で起動するstubは`watchdog!`が親を失ったら、自分のprocess groupをkillする（headlessのstubが親でなくtestを見るのは、testがbackgroundのwrapperをkillして、それが残したturnをruntimeが止めることを確かめるため）。親から切り離して起動する子（testが孤児にするもの）は、ディレクトリの消滅か上限の回数で終わる条件を持たせる。
- `tests/it`のqueueのserviceを起動するtest（`service start`・`supervise`・`up`のどれを通すものも）は、起動するcommandに`tests/common/service.rs`の`OwnedByTest::owned_by_test`を付けるか、testのprocessの中で動くsupervisorには`owned_executable`を実行ファイルに渡す。serviceは`setsid`と二重のforkでtestから離れるので、`Drop`の`service stop`とqueueのディレクトリの消滅だけでは、時間切れの`process::exit`やSIGKILLで終わったtestのserviceが残る（task 1352。仕組みは[Queue service](../design/queue-service.md)の`owner_gone`）。実バイナリのsupervisorのような長く動く子は`KillOnDrop`の下で起動する。

## e2e

- ハッピーパスを`tests/e2e.rs`に置く。実バイナリ・実Gitを使い、Claudeの代わりに受け入れ条件どおりcommitとreceiptを書くstubスクリプトをproviderにする。実cmuxを要るのはinboxを開く`up` / `down`のe2e（`fixture_with_cmux`）だけで、ほかはcmuxの無いhostでも流れる（[ADR-t1433-1](../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)決定3）。`fixture_with_cmux`を使うe2eを足す・名前を変えるときは、関門がcmuxの答えないときに外す`--skip`の一覧（`src/application/install.rs`の`CMUX_E2E`。[ADR-t2105-1](../adr/2026-10-08-t2105-1-e2e-gate-skips-cmux-e2e-only-when-cmux-does-not-answer.md)）も直す（unit testの`the_cmux_e2e_are_the_ones_that_take_the_running_cmux`が漏れを止める）。launchdの`up` / `down`のe2eは使い捨てのHOMEとLaunchAgentのlabelで動いて本番のsupervisorのagentに触れず、launchdを使えないhostでは落ちる（黙ってskipもpassもしない。[`up` / `down`](../design/supervisor-lifecycle/up-down.md)）。どれも`#[ignore]`とし、integrateでは流さない（1本ずつの着地の上限が下がるため。[ADR-t963-1](../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定4）。着地の rebase の後にも流し直さない。
- e2eのrunはworkerのsession wrapperをbackgroundで起動し（runのworkspaceは無い。[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）、harnessはcmuxのworkspaceの一覧でなくwrapperのhandle（processが動いているか）を見て待つ。ケースはrunのsessionがbackgroundのhandleで、wrapperが終わり、`wrapper_launched`のlogが`session.log`であることを確かめ、落ちたtestが残したwrapperはguardが止める。
- e2eを流すのはworkerでなくruntimeがhostで、2つの場所: (1) 差分が`dagq.toml`の`[e2e] paths`に触れるrunと`--evidence e2e`のtaskのrunで、reviewのpassの後・着地の前（[着地の前のe2e](../design/supervisor-lifecycle/landing-e2e.md)、要否は[Validation](../design/supervisor-lifecycle/validation.md)の「runtimeが流すe2e」）。(2) 自動更新とsourceのcheckoutからの`install`が固定バイナリを入れ替える前（[Auto-update](../design/supervisor-lifecycle/auto-update.md)・[install](../design/supervisor-lifecycle/install.md)）。workerが流さないことと`e2e_failed`のresumeでの再現は[手元の検証](local-checks.md)の「e2eを流さない」。
- `[e2e] paths`の外の変更が実cmuxとの組み合わせを壊しても本番のバイナリは(2)の関門が守るので、落ちたらplannerが直すtaskを作る。runをまたぐsupervisorの振る舞いを変え、`tests/e2e.rs`の複数passの筋書きを変えうるtaskは、関門で止まる前に捕まえるため登録の時に`--evidence e2e`を付ける（目安は[taskの登録](task-registration.md)の「e2e」）。
- inboxもplannerもe2eを自分では再実行しない（落ちたときの汎用の手順はpluginの`dagq-recover`の`reference/review-by-hand.md`）。
- 期間限定で登録から外したケース（[ADR-t1582-1](../adr/2026-10-04-t1582-1-temporarily-leave-broker-and-cmux-only-e2e-cases-out.md)）: 今は外しているケースは無い。外したケースは本文を残したまま`#[cfg(any())]`で`tests/e2e.rs`の登録から外れ、`--ignored`でも、(1)の着地の前のe2eでも(2)の関門でも流れない（登録に無いので結果の`skipped`には出ない）。外したケースだけが使うhelperにも同じcfgと、一緒に消す・戻すtaskのIDのcommentを付け、ケースを削除・復帰するtaskは同じ変更でそのhelperのcfgも削除するか外す。crate全体の`dead_code`の許可は付けない。早期returnで成功に見せたり、`#[ignore]`だけで外したりしない。ADRが決めたケースの外に広げない。同じADRで外れた掃除（sweep）のケースは、runのworkspaceが無くなったのでtask 1440がそれだけが使うhelperごと削除した。plannerの`planner::the_runtime_opens_planners_side_by_side_that_submit_go_idle_and_exit`（対話のplannerのworkspace・画面・`/exit`を確かめるケース）は、runtimeのplannerの対話の経路が無くなったのでtask 1441が`tests/e2e/planner.rs`とstubのplannerの分岐ごと削除した（非対話のplannerは`tests/it/planner_headless.rs`が実バイナリのbackgroundのwrapperで確かめる）。

## `[e2e] paths`

`[e2e] paths`（一覧と理由は`dagq.toml`のコメント）を変えるのは、実物の境目のファイルが増えた・分かれたとき。`dagq.toml`なので着地してmain checkoutに反映されてから効く。

## e2eの印

関門が落ちたe2eを流し直し、`.config/e2e-quarantine.toml`の印を効かせる仕組み・書式・上限・効かない印は[Auto-update](../design/supervisor-lifecycle/auto-update.md)、着地の前のe2eで印をどのcommitから読み、どの印を外すかは[着地の前のe2e](../design/supervisor-lifecycle/landing-e2e.md)が持つ（[ADR-t1165-1](../adr/2026-09-30-t1165-1-e2e-gate-reruns-failed-e2e-once-and-records-quarantined-failures.md)、[ADR-t1233-2](../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)決定5）。

- 印は不安定なe2eが直るまでの一時の措置で、`dagq.toml`には置かない。
- 印を付けるのは、直すtaskのIDと期限を持ちplan reviewを通ったtaskだけ。inboxやplannerが手で付けない。
- 印の付いたtestを直すtaskは、同じ変更で自分の印を外す。
- 印を変えるtaskのverifyには`sh scripts/check-e2e-quarantine.sh`を付ける（[taskの登録](task-registration.md)の「推奨の組み合わせ」）。
- 印で通した回数とflakyの回数は`stats`の関門のtestごとの数え方（[stats](../design/supervisor-lifecycle/stats.md)）で見て、直すtaskの優先と印の期限を決める。

## 手動スモーク

実Claudeを含む経路は自動化せず、手動スモーク（[manual-smoke](../design/manual-smoke.md)）で確認する。
