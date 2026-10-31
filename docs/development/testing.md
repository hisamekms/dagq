---
id: development-testing
type: development
title: このrepositoryのtestの制約（coverageと着地の検証・test binary・置き場所・書き方・macOSに固有のtest・CI・判断と境界のtest・ファイルの行数・待ちの上限・e2eとその印・手動スモーク）
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
  - adr-t3762-1
  - development-local-checks
  - development-task-registration
  - development-migrations
  - adr-t920-1
  - adr-t1920-1
  - adr-t2182-1
  - plan-local-checks-history
---

# このrepositoryのtestの制約

testを書く・置く・直すときの今の規則。読むのは、`tests/`・`src/`の`#[cfg(test)]`・`.config/e2e-quarantine.toml`を変えるworkerと、それらを変えるtaskを登録するplanner。手元でどのtestを流すかは[手元の検証](local-checks.md)、taskのverifyの選び方は[taskの登録](task-registration.md)、migrationの規則は[migration](migrations.md)、経緯は[手元の検証とtestの規則の経緯](../plans/local-checks-history.md)とADRが持つ。

## coverage

- 行カバレッジの合計を80%以上に保つ（`cargo-llvm-cov`、行基準、全体）。80%を見る関門はCIだけで、CIが最終関門になる（[ADR-t1925-1](../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)決定1・5）。下回る変更も着地し、CIの見張りが修正taskにする（[CI watch](../design/supervisor-lifecycle/ci-watch.md)）。
- CIの関門のコマンドは`cargo llvm-cov nextest --locked --workspace --fail-under-lines 80`。cargo-nextestがtestを1件ずつ別processで、binaryをまたいで並列に流す（ADR-t1925-1決定8）。dagqは`[workspace]`の無い単一のpackageで、`--workspace`はdagqだけを指し、付けない形と同じ範囲を測る（[ADR-t2126-1](../adr/2026-10-09-t2126-1-single-package-without-workspace-and-coverage-gate-keeps-workspace-flag.md)）。
- testが全部通ってprofileのmergeだけが壊れたprofrawで落ちたときは、CIはllvm-profdataが読めないprofrawだけを除いて同じ80%の関門でreportを出し直す（testの失敗と80%を下回ることは従来どおり落ちる。条件と手順は`.github/workflows/ci.yml`の`cargo llvm-cov nextest`のstepのコメント）。
- 着地の関門は、`integrate`がrebase後にworkerのreceiptを信用せず1回だけ流すtaskの`verification_commands`で（validatingでは実行しない）、そのうちcoverageの関門の行は`dagq.toml`の`[landing_verification]`がunit test全件と影響範囲で絞ったITに置き換える（下の「test binary」）。workerが手元で流すものは[手元の検証](local-checks.md)、taskのverifyの書き方は[taskの登録](task-registration.md)の「coverageの関門」。

## test binary

testは`src/lib.rs`のunit testと`tests/it`・`tests/e2e.rs`・`tests/plugin.rs`にある。このrepositoryのcrateにdoctestは無く、nextestはdoctestを流さない。

- 着地で流れるもの: `sh scripts/landing-it.sh`が1回の`cargo nextest run`で、unit test全件と、CIの夜間の対応表と差分で選んだIT（`tests/e2e.rs`を除く。差分で足した・変えたtestと表に無いtestを必ず含み、表のtreeに無いsrcのファイルと表が多くのITに当てるファイルからはITを選ばずCIに任せ、絞ったITの見込みの時間が上限を超えるとき・共通のファイルに触れたとき・表が取れないか古すぎるときはITを全部）を流し、1件でも落ちれば失敗する。既に落ちているtest（[CI watch](../design/supervisor-lifecycle/ci-watch.md)の一覧）は外し、CIの修正taskのrunでは直す対象のtestを外さない。選び方と値はscriptの冒頭、根拠は[着地のITの絞り込みの測定](../plans/landing-it-selection.md)の「決めたこと」。e2eは別に、要るrunで着地の前にruntimeがhostで流す（[Validation](../design/supervisor-lifecycle/validation.md)の「runtimeが流すe2e」）。
- CIで流れるもの: coverageの関門（上の「coverage」）が全てのtest binaryを全部実行し、1件でも落ちるか80%を下回れば失敗する。着地の検証が見逃した壊れはここで見つかる。

## testの置き場所

- e2e（`tests/e2e.rs`）とplugin（`tests/plugin.rs`）以外のintegration testは1つのtest binary `it`（`tests/it/main.rs`）にまとめ、testは機能ごとのファイル`tests/it/<機能>.rs`に足す。新しいファイルは`tests/it/main.rs`に`mod <機能>;`を足す。`Cargo.toml`は`autotests = false`で`[[test]]`は`it`・`e2e`・`plugin`の3本（[ADR-0078](../adr/0078-one-integration-test-binary.md)）。
- 複数のファイルで使うhelperは`tests/common`（`it`と`plugin`の両方が読む。`tests/it`からは`crate::common`）、runtimeのtestだけが使うfixtureとhelperは`tests/it/runtime_support`（`crate::runtime_support`）に置く。
- portのどのアダプタにも同じ約束を確かめる適合test一式は、アダプタ（とそれを動かすfixture）を引数に取る関数にし、値で動かせる約束は`src/`の`#[cfg(test)]`に、socket・SQLiteの境界を渡る約束は`tests/it`の機能のファイルに置く。各アダプタのtestが一式を呼ぶ（今の一式と項目は[計測の実行環境と資源](../design/measurement-environment.md#適合test)）。

## testの書き方

- 関門ではtestは1件ずつ別processになるので、binaryの中の他のtestとprocessの状態（staticや一度だけの初期化）を共有することに頼るtestを書かない。
- `cargo test`では同じprocessのthreadで走るので、process全体の状態（env・cwd）を変えるtestを書かない。
- testのshellに渡る文字列（stubのagent・reviewerのscript、taskの`verification_commands`、wrapperのscriptなど）にfilesystemのpathを埋めるときは、`'{}'`と単引用符で直接囲まず、`tests/common`の`shell_path`（`dagq::infrastructure::adapters::shell_quote`を包む）で引用する。runtimeのtestのfixtureはqueueとrepositoryを`queue's data.db`・`repo's directory`とapostropheを含む名前で作るので、その下のpathを単引用符で直接埋めると引用が途中で閉じてshellがsyntax errorで終わり、testが意図を確かめないまま通るか不安定に落ちる。SQL・TOMLの文字列リテラルとpathでない文字列は対象外。
- workerのturn（その中で流すtestを含む）では`TMPDIR`がrunの一時ファイルのdir（`<queue>/runs/<run>/tmp`）になり、深くrunのdirの下にある。fixtureのpathが短いこと・`runs/<x>`の下に無いことに頼るtest（queueのdirを掃除・保持・scratchpadの対象として見る、AF_UNIXのsocketをfixtureの下に作る、queueのsocketを別のpathのqueueから探す）は、fixtureを`tempfile::tempdir()`でなく`tests/common`の`short_tempdir`（`/tmp`）に作る。

## macOSに固有のtest

testはmacOS（関門とCIの`checks`）とLinux（CIの`linux`、失敗を通さない）の両方で通す（下の「CI」）。

- まず移植できる形に書く: `ps`・`lsof`・`sed`の出力と引数の違い、`/private/tmp`などのpath、inodeの再利用、時刻の精度（Linuxの`ps`のCPU時間は秒単位、Gitは秒単位で比べることがある）に頼らない。stat dataを変えるならmtimeを秒単位で動かす（`File::set_modified`）。
- macOSにしか無い機能（launchdなど）を使うtestだけを`#[cfg(target_os = "macos")]`で分け、直前のコメントに理由（何がmacOSにしか無いか）を書く。分けたtestの一覧は持たず、`#[cfg(target_os = "macos")]`の付いたtestとその直前のコメントが一覧になる。Linuxで消すのはtestだけで、runtimeの機能は消さない（Linuxで使えない機能は分かるerrorにし、それを`#[cfg(not(target_os = "macos"))]`のtestで確かめる）。
- hostのworker（macOS）はLinuxのtestを流せないので、Linuxの結果は着地後のmainのCIの`linux`のjobのsummaryで読む。Linuxで新しく落ちたtestは、summaryから読んで移植できる形に直すか、上のとおりmacOSに固有として分ける。
- `dagq.toml`がverificationをcontainerにしたとき、着地の検証はLinuxのcontainerで流れ、macOSに固有のtestはbuildされず流れない。hostで流し直すことはせず（workerのコードをcontainerの外で走らせるため）、mainのCIの`checks`（macOS）だけが流し、落ちればCIの見張りが修正のtaskにする（[ADR-t2182-1](../adr/2026-10-14-t2182-1-macos-specific-tests-are-left-to-the-ci-macos-job-not-the-container-verification.md)）。
- containerのworker（resumeを含む）の手元のtestもLinuxで、macOSに固有のtestは流せない。それを変えたworkerは、流せなかったことをreceiptに書き、確かめをmainのCIの`checks`に任せる。

## CI

jobの意味と流すものは`.github/workflows/`の`ci.yml`・`stress.yml`・`ci-failure.yml`の冒頭とjob・stepのコメントが持ち、ここは開発の規則だけを持つ。

- `ci.yml`はmainへのpushとpull requestで流れ、全部のtestを流してcoverageを見る最後の関門になる（[ADR-t1925-1](../adr/2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)）。docsだけの差分ではRustのjob（`checks`と`linux`）を飛ばし、docと`scripts/`の検査のjobは常に流す（[ADR-t2034-1](../adr/2026-10-07-t2034-1-skip-rust-ci-jobs-on-docs-only-changes-and-do-not-read-skipped-runs-as-green.md)）。Rustのjobを飛ばした実行は緑と読まない。mainへのpushのdocsだけかは、直前のcommitではなく`checks`が流れて終わった最新の実行のcommitからの差分で決めるので、実行が取り消されたcodeのcommitの後のdocsだけのpushでもRustのjobは流れる（[ADR-t2134-1](../adr/2026-10-20-t2134-1-push-docs-only-base-is-the-latest-run-whose-rust-checks-ran.md)）。
- mainへのpushの`ci.yml`がworkflow全体で落ちたら、`ci-failure.yml`がlabel `ci-failure`のissueを開くか追記し、次に通ったら閉じる。jobの`continue-on-error`の失敗だけでは開かない。
- `stress.yml`は1日1回、mainで直近に足した・変えたtestを多い周回と高い並列度で繰り返し、落ちたtestごとにlabel `flaky-test`のissueを開くか追記する。workerの手元の[stress](local-checks.md)が止めきれない稀な不安定さを着地の後に見つけるためで、hostの負荷もqueueのslotも使わない（[ADR-t920-1](../adr/2026-09-28-t920-1-light-worker-stress-and-heavy-repetition-in-scheduled-ci.md)決定2）。
- issueから直すtaskを登録するのはplanner。ただし`dagq.toml`に`[ci_watch]`を書いたrepositoryでは、supervisorの見張りが同じmainの失敗を`ci_failure`のfindingにして修正taskを作るので、plannerとinboxは`ci-failure`のissueからtaskを登録しない（見張りが止まっている間も。[ADR-t1920-1](../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)決定8、[CI watch](../design/supervisor-lifecycle/ci-watch.md)）。issueは人に届く知らせとして残る。
- integration testのfixtureのtemplate（`tests/common/template.rs`）は`CARGO_TARGET_TMPDIR`（`target`の`tmp`）に作られる。CIで`target/`をcacheするjobは、cacheの復元の直後と保存の前にそのdirを消す（cacheの後始末がファイルだけを消して中身の無いtemplateを残し、testを落とすため）。jobを足すときも同じstepを置く。

## 判断と境界のtest

[ADR-t1410-1](../adr/2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)。runtimeのtaskのworkerとplannerが守る。

- 状態の判断（状態の遷移・回数と上限・時刻を値で受けた時間の判定・verdictやanswerから操作への対応・askやerrorの文面・次の一手の選び方）は`src/`の副作用のない関数にして`#[cfg(test)]`のunit testで確かめる。unit testは外部プロセス・git・SQLiteのファイル・sleep・実時間の時計を使わない（時刻は値で渡す）。
- `tests/it`はSQLite・Git・プロセス・supervisorの配線・復旧とadoptの境界を代表の1 caseで確かめ、判断のcaseごとにfixtureとsupervisorを起動し直さない。
- 機械で見る部分: `scripts/check-test-rules.sh`が、差分が足した・変えた`src/`の`#[cfg(test)]`の行のうち、行だけで見分けられる実時間の時計・sleep・外部プロセスの形を、runのプログラムのreviewとCIで検出する。見る形と見ない形、1か所を外す`test-rules: allow`の印はscriptの冒頭のコメントが持つ。SQLiteのファイルと、行だけで見分けられない形（ITでの判断のcaseの並べ直しなど）はtest-rulesのreviewが見る。
- e2eは実バイナリ・実Git・実launchdのハッピーパスと境界だけにする（流し方は下の「e2e」）。
- integration testを減らすときは、確かめていた中身をunit testか残すintegration testに対応づけ、行き先の無いまま消さない。
- 時間の関門（[ADR-t1707-1](../adr/2026-10-05-t1707-1-time-gate-for-added-or-changed-integration-tests.md)）: baseからの差分で足した・本文を変えた`tests/it`のtestの1本の時間が閾値を超えたら、許可の一覧に項目が無いかぎり関門が落ちる。許可の一覧は`.config/it-slow-allow.toml`と`.config/it-slow-allow.d/*.toml`を合わせたもので、新しい項目は足すtaskのファイル`.config/it-slow-allow.d/t<task ID>.toml`に足す（別々のtaskの追加が同じファイルに触れないため。`.config/it-slow-allow.toml`には足さず、そこの既存の項目を直す・外すのはよい）。閾値・許可の一覧の書式・testの名前の求め方・scriptの引数は`scripts/check-it-test-time.sh`の冒頭のコメントが持つ。
  - 超えたら、まずtestを直す（判断をunit testへ移す、caseごとにfixtureとsupervisorを起動し直さない）。直さずに許可の一覧に項目を足してよいのは、理由が次のどちらかのときだけ: (a) そのtestが守る境界（SQLite・Git・プロセス・supervisorの配線・復旧とadoptのどれか）を書ける、(b) 移し替えの予定があり、その行き先のtaskかgoalを書ける。判断のcaseを並べただけのtestは(a)に当たらない。項目には足したtaskのIDを書く。
  - 項目のtestを直すか移すtaskは、同じ変更で自分の項目を外すか理由を直す。testを消すか名前を変えるtaskは、消えた名前の項目を同じ変更で外す（残すと、後で同じ名前のtestを足したときに関門を素通りする。scriptはHEADに無い項目を警告するだけで落とさない）。
  - 閾値は本番の関門の`dagq::it`の1本の平均の数倍に置く。CIのrunnerと本番の関門は並列度が違い同じtestの秒が違うので、CIで落ちたtestが本番の関門のlogの秒では閾値の内なら、その秒を添えて直すか項目を足すかをplannerが決める。
  - 関門の場所: workerの手元（[手元の検証](local-checks.md)の「itのtestの時間の関門」）、runのreviewのtestの規則の検査、CIのpushとpull_request（mainへのpushで落ちればci-failureのissue）。`integrate`の検証には足さない。
  - helperやfixtureだけの変更は対象にならず（それを使うtestが遅くなっても捕まえない）、CIのrunnerと本番の関門では秒が違う。
- 境界の関門（[ADR-t3762-1](../adr/2026-10-31-t3762-1-integration-tests-added-by-a-diff-declare-the-boundary-they-keep.md)、[ADR-t3802-1](../adr/2026-10-31-t3802-1-integration-test-boundaries-in-a-named-registry-with-a-module-ratchet.md)）: `tests/it`が守る境界は、境界のid・境界の種類・代表のtest・理由・足したtaskを持つ登録簿にある。速さに依らず判断のcaseを並べたITと同じ境界の2本目が足されるのを止めるため、`scripts/check-it-added-tests.sh`がbaseからの差分について下の3つを落とす。登録簿は`.config/it-boundaries.d/*.toml`を合わせたもので、項目は足すtaskのファイル`.config/it-boundaries.d/t<task ID>.toml`に足す（時間の関門の許可の一覧と同じく、別々のtaskの追加が同じファイルに触れないため）。書式・境界の種類の値・idの形・新しいtestと移したtestの見分け方・本数の数え方・baseの求め方はscriptの冒頭のコメントが持つ。
  - 新しく現れたtestは、既存のidの代表と入れ替える（そのidの代表をこのtestに変え、元の代表は消すか別のidの代表にするかunit testへ移す）か、新しいidを足さなければ落ちる。移しただけのtestと既存のtestの本文の変更は対象にならない。
  - 1つのidを代表に持つtestが2本以上（R）なら落ちる。2本目は判断をunit testへ移すか、別の境界なら別のidにする。
  - module（`tests/it`のファイル）ごとの`#[test]`の本数をbaseとwork treeで数え、新しいid・代表の入れ替え・移したtestで説明できない増加があれば落ちる（入れ替えた元の代表を残したときも）。数は保存せず毎回baseから数えるので、並行のtaskで衝突しない。
  - idを持てるのは、そのtestが上の境界（SQLite・Git・プロセス・supervisorの配線・復旧とadoptのどれか）を代表の1 caseで確かめ、ITでしか確かめられない理由を`reason`に書けるときだけ。判断のcaseを並べただけのtestはidを持たず、判断を`src/`の副作用のない関数のunit testへ移す。
  - 新しいidの承認: 項目に足したtaskのIDを書き、そのtaskのacceptanceかdescriptionがidを名指していること（plan reviewが見る）と、`reason`が境界を書いていること（test-rulesのreviewが見る）で承認とする。機械は項目の形と数だけを見る。
  - 移行の期間: 古い置き場`.config/it-boundary.d/*.toml`の古い書式の項目（`name`・`boundary`・`reason`・`task`）も、そのtestを代表に持つ新しいid1つとして読む。登録簿より前の規則で登録されたtaskのrunがそこに書くのを通すためで、新しい項目は登録簿に書く。古い置き場を読むのをやめるのは後の退役のtask。
  - 関門の場所: workerの手元（[手元の検証](local-checks.md)の「testの規則の点検」）、runのプログラムのreview、CIのpushとpull_request。`integrate`の検証には足さない（時間の関門と同じく、着地の時間を増やさないため）。
  - testを消すか名前を変えるtaskは、そのtestを代表に持つ項目を同じ変更で直す（scriptは無いtestの代表を警告するだけで落とさない）。

## testファイルの行数

- `tests/*.rs`・`tests/common/*.rs`・`tests/it/**/*.rs`（tests/の下の全`.rs`）はどれも3,000行以下に保つ（1つのbinaryにまとめてもファイル単位のまま）。末尾にtestを足す形の大きなファイルは別々のtaskの追記が同じ場所で衝突するため。
- `scripts/check-test-file-lines.sh`が超えたファイルを名前と行数つきで出してexit 1にし、CIも実行する。
- 超えそうなら、testを足す前に機能ごとのファイル（`tests/it/runtime_*.rs`・`tests/it/lifecycle_*.rs`のように）へ分け、複数のファイルで使うhelperは`tests/common`に、runtime系だけのものは`tests/it/runtime_support`に寄せる。
- `src/`はまだ対象外。

## 待ちの上限

- testの待ちには上限を付ける。pollのloopはdeadlineを持ち、上限の無い待ち（threadのjoin、stubのsessionの終了、`dagq`の子プロセスの`output()`）は`tests/common/mod.rs`の`within`（fixtureが持つtest全体の`common::test()`と、1つの待ちの`STEP_LIMIT`）で包む。
- 上限を過ぎるとtest binaryがtestの名前と待っていた条件をstderrに出してexit 101で失敗する（`cargo test --test it`では同じbinaryの残りのtestもそこで止まる。関門のnextestはtestごとのprocessなので止まるのはそのtestだけ）。`cargo test | tail`が戻らなくなることはない。
- e2eのbinary（`tests/e2e.rs`）は、関門の時間切れ（compileを含むe2e全体に掛かる`--e2e-timeout`）のkillより先に、止まった待ちの名前で落ちるように期限を入れ子にする（`tests/common`の`deadlines`で選ぶ。判断は`src/domain/e2e_deadline.rs`）。processの期限は最初のfixtureの開始から測り、各testの期限はそれより前に、testの中の待ちとpollのloopの期限はtestの期限より前に切る。testがいつ始まってもprocessの期限は越えず、どこで止まっても限度を超えた待ちの名前と開いている他の待ちが出る。時間切れの後始末は、hookごとの上限を全体の残りで切って新しい順に走らせ、全体の期限を過ぎたら残りのhookを走らせない。終わったhook・終わっていないhook（そのthreadは残す）・走らせなかったhookを名前で出し、exit 101で終わる。残したprocessは、関門がe2eのprocess groupに送るSIGKILLと、e2eの`TMPDIR`の後始末が止める。
- この保証は、cargoのcompileとharnessの起動が`tests/e2e.rs`の`RUN_LIMIT`の説明にある予算の内に収まるときだけ成り立つ。予算は測定から決めた経験的な値で、compileの時間は制限しない。予算を超えれば今までどおり関門の時間切れでkillされ、timeoutと記録される。それより前のexit 101は、e2eの失敗（harnessの要約が無いのでやり直しの対象外）と記録される。exit 101と関門のkillはどちらもharnessの最終行を残さないので、予算を超えたかは関門の記録からは分からない。`tests/it`は期限を選ばず、待ちは自分の上限だけを持ち、後始末は全部のhookをhookごとの上限で新しい順に走らせる。
- testが起動するシェルの待ち・ループ・stub（taskのverification、stubのagentやturnのscript、reviewerやjobのscript、wrapperのscript、`#[cfg(test)]`の子プロセス）は、testが成功・失敗・panic・時間切れのどれで終わっても終わる形にする。testが書くファイルを待つときは`while [ ! -f ... ]`を直接書かず、`tests/common`の`await_file`（シェルの単語を待つ）か`await_path`（pathを引用して待つ）を使う。`headless_claude`と`headless_codex`のstub（scriptの前置きの`TURN_PRELUDE`・`RESUME_TURN_PRELUDE`をその中で読む）は同じシェル関数`await_file`を定義しているので、その中では`await_file "$EXIT.go"`と書く。この待ちは、待つファイルのディレクトリ（testの`TempDir`）が無くなるか、testのprocess（headlessのstubでは`STUB_TEST_PID`、それ以外はシェルの親）が居なくなると抜ける。
- 機械で見る部分: `scripts/check-test-rules.sh`が、差分が足した・変えた`tests/`の行の自前のファイル待ちの形を、runのプログラムのreviewとCIで検出する（見る形と`test-rules: allow`の印はscriptの冒頭のコメント）。上限の無いthreadのjoin・`Barrier`・`output()`のように行だけで見分けられない待ちはtest-rulesのreviewが見る。
- 無限ループ（`while :`）や長い`sleep`を持つ子は、親を失ったら（`kill -0 "$PPID"`が失敗したら）終わる条件か、`Drop`で子かそのprocess groupをkillするguardの下に置く。`Drop`のguardだけでは足りない: 時間切れの`process::exit`は`Drop`を通らないので、同じkillを`tests/common`の`on_timeout`に登録する（`KillOnDrop`と`runtime_support`の`Fixture`の形）か、子に親の死で終わる条件を持たせて組にする。headlessのstubは`HEADLESS_WATCHDOG`が、testのprocess（`STUB_TEST_PID`）かstubのディレクトリが無くなったら、`StubSpawner`で起動するstubは`watchdog!`が親を失ったら、自分のprocess groupをkillする（headlessのstubが親でなくtestを見るのは、testがbackgroundのwrapperをkillして、それが残したturnをruntimeが止めることを確かめるため）。親から切り離して起動する子（testが孤児にするもの）は、ディレクトリの消滅か上限の回数で終わる条件を持たせる。
- `tests/it`と`tests/e2e.rs`のqueueのserviceかsupervisorを起動するtest（`service start`・`supervise`・`up`のどれを通すものも）は、起動するcommandに`tests/common/service.rs`の`OwnedByTest::owned_by_test`を付けるか、testのprocessの中で動くsupervisorには`owned_executable`を実行ファイルに渡す。serviceは`setsid`と二重のforkでtestから離れ、`KillOnDrop`は時間切れの`process::exit`は覆ってもSIGKILLは覆わないので、`Drop`の`service stop`とqueueのディレクトリの消滅だけでは、そう終わったtestのserviceと実バイナリのsupervisorが残る。ownerを付けたserviceと`supervise`はtestのprocessが居なくなれば自分で止まり、`up`は付いたownerをlaunchdで起動するsupervisorに渡す（仕組みは[Queue service](../design/queue-service.md)の`owner_gone`）。実バイナリのsupervisorのような長く動く子は`KillOnDrop`（`tests/e2e.rs`では`ChildGuard`）の下で起動する。
- queueのfixtureの一時ディレクトリは、testが終わった後に`$TMPDIR`にも`/tmp`にも残さない。`runtime_support`の`Fixture`と`ContainerQueue`の`Drop`は、stubをkillした後、testのprocessのthreadで動くsession wrapper（`headless::launch_background`）にexit requestを書いてその終わりを待ち、それからディレクトリを消す（wrapperがkillされたturnのidle markerを消している最中のディレクトリに書くと、`remove_dir_all`が途中で落ちて`runs/<run>/idle.json`だけが残る）。exit requestはrunのファイルの書き込み（`LocalRunFiles`）で書き、testがrunのディレクトリに残したlinkを辿ってfixtureの外を変えない。この待ちも`within`の上限（`STEP_LIMIT`）を持ち、過ぎれば他の待ちと同じくtest binaryが失敗し、dropはwrapperが動いたままディレクトリを消さない。時間切れの後始末（`remove_at_timeout`）も、wrapperが止まなければディレクトリを丸ごと残し、processの終わりで外れるownerのlockを次のfixtureのsweepが取って消す。testのthreadの外でfixtureのディレクトリに書くものを足すときは、同じくdropが待てる形にする。
- `Drop`も`on_timeout`も通らない終わり方（nextestの時間切れや取り消しのSIGTERM・SIGKILL）で残るディレクトリは、次のtestのprocessが消す。queueのfixture（`runtime_support`の`fixture`・`ContainerQueue`・`plan_review`の`fixture`・`tests/common/service.rs`の`queue`・e2eの`fixture`）は`tests/common/cleanup.rs`の`owned_temp_dir`（`/tmp`に置くものは`owned_short_temp_dir`）か`claim_fixture_dir`でディレクトリにownerのlockを持ち、そう作るprocessの最初のfixtureが置き場（`$TMPDIR`か`/tmp`）ごとに（e2eでは`sweep_abandoned_fixtures`を呼ぶtestが）、ownerの居ないディレクトリ（lockが取れるもの）と、ownerのfileを持たない前のbinaryのfixtureの形で1時間より古いもの（形は`cleanup.rs`の`unmarked_fixture`）を、中で動くprocessごと消す。queueのfixtureを新しく作るときもこの形にする。失敗の調べのためにfixtureを残す仕組みは置かない。

## e2e

- ハッピーパスを`tests/e2e.rs`に置く。実バイナリ・実Gitを使い、Claudeの代わりに受け入れ条件どおりcommitとreceiptを書くstubスクリプトをproviderにする。関門はhostの道具の有無でe2eを外さない（[ADR-t2159-1](../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定7）。launchdの`up` / `down`のe2eは使い捨てのHOMEとLaunchAgentのlabelで動いて本番のsupervisorのagentに触れず、launchdを使えないhostでは落ちる（黙ってskipもpassもしない。[`up` / `down`](../design/supervisor-lifecycle/up-down.md)）。どれも`#[ignore]`とし、integrateでは流さない（1本ずつの着地の上限が下がるため。[ADR-t963-1](../adr/2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定4）。着地の rebase の後にも流し直さない。
- e2eのrunはworkerのsession wrapperをbackgroundで起動し（runのworkspaceは無い。[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）、harnessはwrapperのhandle（processが動いているか）を見て待つ。ケースはrunのsessionがbackgroundのhandleで、wrapperが終わり、`wrapper_launched`のlogが`session.log`であることを確かめ、落ちたtestが残したwrapperはguardが止める。
- e2eを流すのはworkerでなくruntimeがhostで、2つの場所: (1) 差分が`dagq.toml`の`[e2e] paths`に触れるrunと`--evidence e2e`のtaskのrunで、reviewのpassの後・着地の前（[着地の前のe2e](../design/supervisor-lifecycle/integrate.md#着地の前のe2e)、要否は[Validation](../design/supervisor-lifecycle/validation.md)の「runtimeが流すe2e」）。(2) 自動更新とsourceのcheckoutからの`install`が固定バイナリを入れ替える前（[Auto-update](../design/supervisor-lifecycle/auto-update.md)・[install](../design/supervisor-lifecycle/install.md)）。workerが流さないことと`e2e_failed`のresumeでの再現は[手元の検証](local-checks.md)の「e2eを流さない」。
- `[e2e] paths`の外の変更が実プロセス・launchdとの組み合わせを壊しても本番のバイナリは(2)の関門が守るので、落ちたらplannerが直すtaskを作る。runをまたぐsupervisorの振る舞いを変え、`tests/e2e.rs`の複数passの筋書きを変えうるtaskは、関門で止まる前に捕まえるため登録の時に`--evidence e2e`を付ける（目安は[taskの登録](task-registration.md)の「e2e」）。
- inboxもplannerもe2eを自分では再実行しない（落ちたときの汎用の手順はpluginの`dagq-recover`の`reference/review-by-hand.md`）。
- 期間限定で登録から外したケース（[ADR-t1582-1](../adr/2026-10-04-t1582-1-temporarily-leave-broker-and-cmux-only-e2e-cases-out.md)）: 今は外しているケースは無い。外したケースは本文を残したまま`#[cfg(any())]`で`tests/e2e.rs`の登録から外れ、`--ignored`でも、(1)の着地の前のe2eでも(2)の関門でも流れない（登録に無いので結果には出ない）。外したケースだけが使うhelperにも同じcfgと、一緒に消す・戻すtaskのIDのcommentを付け、ケースを削除・復帰するtaskは同じ変更でそのhelperのcfgも削除するか外す。crate全体の`dead_code`の許可は付けない。早期returnで成功に見せたり、`#[ignore]`だけで外したりしない。ADRが決めたケースの外に広げない。非対話のplannerは`tests/it/planner_headless.rs`が実バイナリのbackgroundのwrapperで確かめる。

## `[e2e] paths`

`[e2e] paths`（一覧と理由は`dagq.toml`のコメント）を変えるのは、実物の境目のファイルが増えた・分かれたとき。`dagq.toml`なので着地してmain checkoutに反映されてから効く。

## e2eの印

関門が落ちたe2eを流し直し、`.config/e2e-quarantine.toml`の印を効かせる仕組み・書式・上限・効かない印は[Auto-update](../design/supervisor-lifecycle/auto-update.md)、着地の前のe2eで印をどのcommitから読み、どの印を外すかは[着地の前のe2e](../design/supervisor-lifecycle/integrate.md#着地の前のe2e)が持つ（[ADR-t1165-1](../adr/2026-09-30-t1165-1-e2e-gate-reruns-failed-e2e-once-and-records-quarantined-failures.md)、[ADR-t1233-2](../adr/2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)決定5）。

- 印は不安定なe2eが直るまでの一時の措置で、`dagq.toml`には置かない。
- 印を付けるのは、直すtaskのIDと期限を持ちplan reviewを通ったtaskだけ。inboxやplannerが手で付けない。
- 印の付いたtestを直すtaskは、同じ変更で自分の印を外す。
- 印を変えるtaskのverifyには`sh scripts/check-e2e-quarantine.sh`を付ける（[taskの登録](task-registration.md)の「推奨の組み合わせ」）。
- 印で通した回数とflakyの回数は`stats`の関門のtestごとの数え方（[stats](../design/supervisor-lifecycle/commands.md)）で見て、直すtaskの優先と印の期限を決める。

## 手動スモーク

実Claudeを含む経路は自動化せず、手動スモーク（[manual-smoke](manual-smoke.md)。過去の結果は[manual-smoke-results](../plans/manual-smoke-results.md)）で確認する。
