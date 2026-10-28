---
id: design-toolchain-profiles
type: design
title: Toolchain profiles
status: current
created: 2026-10-12
scope: runtime
tags:
  - runtime
  - conventions
related:
  - adr-t2268-1
  - adr-t639-1
  - adr-t768-1
  - adr-t2257-1
  - adr-t614-1
  - design-supervisor-lifecycle-integrate
  - design-supervisor-lifecycle-validation
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-source-repository
---

# Toolchain profiles

言語・道具ごとの知識は、runtimeに組み込んだtoolchainのprofileの実装だけが持ち、projectは`dagq.toml`で使うprofileを宣言する（[ADR-t2268-1](../adr/2026-10-12-t2268-1-toolchain-knowledge-behind-declared-profiles.md)）。
runtimeのほかのコードは、宣言したprofileをportを通して呼ぶだけで、道具の出力の印やenvの名前を持たない。

**実装状況**: portと登録とRustのprofile、`[toolchains.<name>]`の宣言、検証の出力の読み方（失敗の分類・落ちたtestとflakyのtestの抜き出し・flakyのやり直しのenv）、CIの見張りのJUnitのtestの名前、hostの負荷のprocessの種類、hostのRustのtoolchainの記録とKPIの`toolchain`の軸、landing recheckと自動更新のbuildの出力の置き場と掃除、runのworktreeのビルド成果物の掃除、fileの中のtestの行の見分け、推奨の設定（宣言・検証のコマンドの組み合わせ・reviewのプログラム・containerの材料・registryのhost・workerのcacheのdir）、計測のコマンドの分類の既定の規則はある。

## portの責務

- portは`domain::toolchain::ToolchainProfile`の1つのtraitで、1つの実装が1つの言語の道具の知識を持つ。
  各methodの意味と、runtimeが呼ぶ順はそのdoc commentが持つ。
- 今のmethodは、検証のコマンドの出力を読むもの（道具の印から失敗を既存の分類に当てること、落ちたtestと流し直しで通ったtestの名前を読むこと、flakyのやり直しで足すenvを返すこと）と、CIと計測を読むもの（CIの見張りがJUnitのtestを道具の表示と同じ名前にする作り方、hostの負荷で道具のprocessをbuildの道具かcompilerに分けること）と、buildの出力の置き場を返すもの（下の「buildの出力の置き場」）と、推奨の設定を返すもの（下の「推奨の設定」）と、fileの中で差分の行がtestの中かを見分けるもの（`test_lines`）である。
  ほかに、計測のコマンドの形が平文で持つ道具の語（既知のサブコマンドと、値を取らないoptionと値を取る既知のoptionの名前と、そのprofileに固有の代入の名前）を返すものがある（[ADR-t1673-1](../adr/2026-10-21-t1673-1-command-shape-keeps-only-known-subcommands-in-plain.md)、[ADR-t3201-1](../adr/2026-10-24-t3201-1-command-shape-keeps-only-listed-flags-and-variables-in-plain.md)）。
  processの種類の列は名前を固定し（`cpu_cargo`・`cpu_rustc`ほか）、profileは閉じた型（`ToolProcess`）でどちらの列かを返す。
  claude・dagq・そのほかは言語に依らないruntimeの共通の分類である（[hostの負荷](supervisor-lifecycle/host-metrics.md)）。
- 計測のコマンドの分類の既定の規則もprofileが返す（`command_rules`）。
  道具の知識だけを持ち、projectのtest targetのような名前を持たない。
  宣言したprofileの規則は`dagq.toml`の`[[telemetry.commands]]`の後、runtimeの共通の規則の前に宣言の順で当たる（[計測のコマンドの形と分類](measurement-commands.md)）。
- `test_lines`はreviewが手元の関門の要否を決めるときに使い、pathsがそのfileに当たる宣言（pathsの無い宣言は常に当たる）のprofileだけに宣言の順に聞く（`Toolchains::test_lines`、[ADR-t2589-1](../adr/2026-10-17-t2589-1-local-check-runs-are-recorded-by-project-scripts-and-matched-to-the-receipt-tree.md)決定6）。
  当たるprofileが無いか、どれも見分けないfileでは、変えた行をtestの行とみなす（関門を省かない側）。
- hostのRustのtoolchainの記録（claimの`rustc_release`・`rustc_host`）とKPIの`toolchain`の軸と`stats`の`rustc`の欄は、宣言したprofileにRustのprofileがあるときだけ働く（`Toolchains::records_rust_toolchain`）。
  記録の欄の名前がRustのものなので、ほかのprofileはtoolchainの版を返さない。
- profileが返せる分類は型で閉じている: hostの分類は`killed`と`timeout`だけ（`ToolHostClass`）、コードの分類はbuild・lint・test・format・coverageだけ（`ToolCodeClass`）。
  profileは既存の分類に印を足すだけで、hostの分類の集合（`FailureClass::is_environmental`）を広げられない（[ADR-t639-1](../adr/2026-09-27-t639-1-retry-verification-failures-of-the-host-once.md)決定5）。
- Rustのprofile（`domain::toolchain::rust`）は、cargo・cargo test・cargo-nextest・clippy・rustfmt・cargo-llvm-covの印、nextestの失敗のstatusの集合、flakyのやり直しのenvと1回目に受け継いだ値を外す理由を、定義のそばのdoc commentに持つ。
  同じstatusの集合を持つscriptとCIをそろえて直す約束もそこにある。

## 共通の分類との分担

- runtimeの共通の分類（`domain::verify_failure::classify`）は、言語に依らない印を宣言に依らず読む: 容量の不足のerrorとerrno、signalとshellの128+signalの終了コード、時間切れの終了コード124、コマンド全体の上限（`CommandTimedOut`）。
  宣言の無いrepositoryでも、宣言したprofileが当たらない出力でも、integrateのhostの失敗のやり直しと保留はこれで働く（[`integrate`](supervisor-lifecycle/integrate.md)）。
- 印を当てる順は共通の分類が持ち、profileはその順の決まった段で呼ばれる。
  hostの原因を先に見て、流し直しで通ったtestを時間切れより先に見る、という約束は`classify`のdoc commentにある。
- 「落ちたtestが全て流し直しで通った」の判定（`flaky`）は共通の分類が行い、profileは落ちたtestと通ったtestの名前と、それを示す行だけを返す。
- 分類したprofileの名前は`VerifyFailure::profile`に残り、flakyのやり直しはそのprofileのenvだけを足す。
- projectに固有の印（このrepositoryの`tests/common`の`within`の時間切れの行など）はprofileにも共通の分類にも置かず、`dagq.toml`の`[verify_marks.<name>]`が宣言する（`domain::verify_marks`、ADR-t2268-1決定2）。
  印が当てられる分類は型で閉じ（`MarkClass`はprofileと同じ`ToolHostClass`と`ToolCodeClass`）、分類の集合を広げない。
  当てる段はprofileの印と同じで、各段の順は`classify`のdoc commentが持つ。

## 宣言が効く範囲と当て方

- 宣言は`dagq.toml`の`[toolchains.<name>]`で、書式・各keyの意味・既定・誤りの扱いは`infrastructure::run_env::TOOLCHAINS_PREFIX`のdoc commentが持つ。
  読み込みは`run_env::load_toolchains`、解決した結果は`domain::toolchain::Toolchains`で、runtimeは`Verifier::toolchains`で受け取る。
- 検証の出力・CIのJUnit・hostのprocessのようにpathに結びつかない出力は、宣言した全てのprofileを宣言の順に当て、各段で最初に当たったものを取る（ADR-t2268-1決定6）。
  宣言したprofileのどれもJUnitを読まなければ、CIの見張りはtestの名前を読まずjobとstepで知らせ、どれも知らない道具のprocessは`other`に数える。
  検証のコマンドは差分のpathに依らずrepository全体を流すので、pathsで絞ると分類を落とすためである。
  pathsはpathに結びつくもの（buildの出力の置き場、testの行の見分け、推奨の案）のためにある。
  推奨の案では、`recommend::propose`が宣言のpathsを`dagq init`とbuilt-inのreviewの生成コマンドの宣言の案に写す（[推奨の設定](#推奨の設定)）。
- 宣言したprofileがどれも読まない出力は、共通の印にも宣言したprojectの印（`[verify_marks.<name>]`）にも当たらなければ`unknown`で、落ちたtestの名前は無く、flakyのやり直しのenvも足さない。
- 宣言が無いrepositoryでは、dagqのソースでもどのprofileも働かない（`Toolchains::none`、ADR-t2268-1決定3）。
  検証の出力は共通の印と宣言したprojectの印だけが読み、flakyのやり直しのenvもbuildの出力のdirも足さず、runのworktreeのビルド成果物を消さず、CIの見張りはjobとstepで知らせ、hostの`rustc`とtoolchainの記録とKPIの`toolchain`の軸と`stats`の`rustc`の欄は出さない。
  `doctor`と`status`は宣言の無いことを空の`declared`で示し、警告しない。
  `Cargo.toml`などからprofileを推測して有効にすることはしない。
- 読むのはqueueが束縛されたmain checkoutの作業ファイルの`dagq.toml`で、`integrate`とlanding recheckが検証のたびに読む。
  workerのtranscriptとCodexのturnの計測も同じ解決で、`[telemetry] failed_tests`を有効にしたときだけworkerが流したコマンドの出力から落ちたtestの名前を読む（`run_env::measured_toolchains`）。
  計測は検証と違って止めず、`dagq.toml`が読めない・誤りのときはどのprofileでも読まず、sessionは落ちたtestの名前なしで記録する。
  着地の外の読み手（CIの見張りの確かめ、hostの負荷のsample、claim、KPI）も使うたびに同じ解決で読み、`dagq.toml`が読めない・誤りのときは計測と同じくどのprofileでも読まない（`run_env::toolchains_in_use`）。
- 組み込みに無いprofileの名前は設定の誤りで、`integrate`は検証の前に着地処理のエラーにし、`doctor`と`status`は`toolchains`に`error`を出す（`domain::toolchain::Toolchains::view`）。
- 旧いバイナリは`[toolchains.<name>]`を知らない表として拒むので、repositoryが宣言を足すのは固定バイナリが読めるようになってからにする（[Run environment](supervisor-lifecycle/run-environment.md)）。

## buildの出力の置き場

- runtimeが自分のdirでコマンドを流すとき（landing recheckの`recheck/`と、自動更新のsource buildの`update/`）、buildの出力のdirの名前と、それを指すenvの名前はprofileだけが返す（`ToolchainProfile::build_output`、`BuildOutput`）。
  runtimeはprofileが返したdirを宣言ごとに分けて作り、掃除する（ADR-t2268-1決定6）。
  profileと同じ名前の宣言はprofileのdirそのものを使う（決定9）。
  ほかの宣言のdirの名前と決め方は`BuildOutput::declared`のdoc commentが持つ。
- landing recheckは、hostのscratch worktreeで流すとき、pathsがmergeした木のファイルに当たる宣言（pathsの無い宣言は常に当たる）の出力を使う（`Toolchains::build_runs`）。
  1回の実行でenvは1つの値しか持てないので、同じenvの宣言が複数当たればcommandをその数だけ流し、当たる宣言はどれも1回の実行で自分のdirを使う。
  当たる宣言が別々のenvなら、commandは1回である。
  当たる宣言が無ければenvを足さずdirも作らず、commandは`[run.env]`のとおりに1回流れる。
  containerの中で流すとき（`recheck::run_in_container`）は宣言ごとのbuildの出力を使わず、commandは検証のcontainerで1回流れる。
- 空き容量のための掃除は、今の宣言に依らず、組み込みのprofileのどれかの宣言のdirに当たる`recheck/`の下のdirを全て見る（`recheck::build_dirs`）。
  宣言を変えた後に前の宣言のdirが残らないためである。
- 自動更新のsource buildはdagq自身の事情なので、宣言に依らずRustのprofileの出力を使う（[Auto-update](supervisor-lifecycle/auto-update.md)）。
- runのworktreeの掃除（[Run worktrees](supervisor-lifecycle/run-worktrees.md)）が消すworktree直下のdirの名前もprofileだけが返し（`ToolchainProfile::worktree_build_dirs`）、掃除はlanding recheckと同じ解決の結果に入っているprofileの名前を宣言のpathsに依らず全て消す（理由は`Toolchains::worktree_build_dirs`のdoc comment）。
  当たるprofileが無いときと、`dagq.toml`が読めない・誤りのときは消さない。
- buildの止まっているrunのworktreeから、そのdirの中で次のbuildが読まない古いcacheだけを消すとき（[Run worktrees](supervisor-lifecycle/run-worktrees.md#終わっていないrunのビルド成果物)）、何が古いかもprofileだけが返す（`ToolchainProfile::stale_build_entries`、`StaleEntries`）。
  runtimeはprofileに渡した読み取りの関数でdirの名前を読ませ、返されたentryを消して測るだけで、名前の規則を持たない。
  既定は何も返さず、Rust以外のprofileは何も消させない。
  Rustのprofileは`<profile>/incremental/<crate>-<hash>/`ごとに、`-working`で終わらないsessionのdir（`s-<T>-<R>-<SVH>`）を名前の`<T>`（固定長の36進なので文字列の順が時刻の順）で並べ、最新の1つより古いものと対のlock（`s-<T>-<R>.lock`）を返す。
  作業中のsession、それと同じ`s-<T>-<R>`のlock、dirの無いlockは、rustcの後片付けに任せて返さない。
  規則はrustcの`compiler/rustc_incremental/src/persist/fs.rs`のもので、版で分けない（確かめた版と根拠は`domain::toolchain::rust::incremental`のdoc comment）。
- workerと`integrate`のbuildの出力の置き場は`[run.env]`のprojectの値で、profileは関わらない。

## 推奨の設定

- profileは推奨の設定を`ToolchainProfile::recommendation`で返す（`domain::toolchain::recommend`、ADR-t2268-1決定7）。
  種類は、宣言を推測する印のfileの名前、taskの`--verify`に渡す検証のコマンドの組み合わせ、reviewのプログラム（`[review.programs.<name>]`のtableとscriptの中身）、containerの設定（projectのimageのContainerfileの行と入力のfile、registryのhostとその理由、workerのcacheのdir）である。
  containerの設定は[Container runtime](container-runtime.md)が読む`[container.image]`・`[egress]`・`[container.worker_cache]`の形の案にし、複数のprofileの分は1つのContainerfileと各表に重ねずにまとめる。
  imageの案は、推奨の検証のコマンドが呼ぶ道具を入れ、buildの中でそれぞれを呼んで確かめるので、案のimageで推奨の検証が流せないときはbuildが落ちる。
  imageの案は入力のfileがrepositoryにある前提で、無いときに合わせて変えるのは書く人である（推奨はfileの有無を見ない）。
  workerのcacheの案は、道具が共有のdataを守るlockのfileを含むdirにする。
  containerのroot filesystemは読み取り専用で、lockだけがその外に残ると道具はlockなしで進み、同じqueueのworkerが同じdataを同時に書く。
  値はその実装の定義のそばに置く。
- 推奨はruntimeの既定にしない。
  `dagq.toml`に書かれるまで、どのprofileも働かず、reviewのプログラムも無く、imageはbaseだけ・registryもworkerのcacheも無い（ADR-t2257-1決定1・3）。
- 案にするprofileは、`dagq.toml`の宣言があればそれ、無ければrepositoryのfileの推測である（`recommend::propose`）。
  推測は印のfileがrootにあれば全体、下にだけあればそのdirごとの`<dir>/**`をpathsにし、宣言の案に使うだけで有効にしない。
  宣言が読めない・組み込みに無いprofileを名指す・fileの一覧が取れないときは、案を出さずにその誤りを出す。
- `dagq init`はmain checkoutの作業ファイルの`dagq.toml`と作業ツリーのfileから案を作り、出力の`recommended.toolchains`に宣言の案・`--verify`・reviewのプログラム・containerの設定とContainerfileの案・それらを並べた`dagq.toml`の案の文を出すだけで、`dagq.toml`もContainerfileも書かない。
- `dagq setup-review`はHEADのcommitの`dagq.toml`とfileから案を作る。
  jobに渡すのは、そのコマンドが書いてよい範囲（`.dagq/review/programs/`の下のscript）に入るreviewのプログラムだけで、範囲の外のscriptの推奨は渡さず案内にだけ出す（`RecommendedProgram::within_setup`）。
  宣言の案とcontainerの設定はreviewの外のtableで、`--verify`は`dagq.toml`のkeyでなくtaskの登録の値なので、どれもjobに渡さず出力の案内にだけ出す。
  生成の後の変更の範囲の検査は推奨に依らず同じで、推奨が書く範囲を広げない（[setup-review](supervisor-lifecycle/setup-review.md#境界と不変条件)）。

## 新しいprofileの足し方

- `ToolchainProfile`を実装する型を1つ書き、`domain::toolchain::BUILTIN`の登録に1行足す。
  runtimeのほかのコードは変えない（ADR-t2268-1決定8）。
- 印の一覧と道具の版、やり直しのenvの意味は、その実装の定義のそばのdoc commentに書き、designに列挙しない。
- 外部のスクリプトや動的な読み込みでprofileを足す仕組みは無い。

## テスト

- 偽の2つ目のprofileを実装と登録だけで足し、分類・落ちたtestの抜き出し・やり直しのenv・JUnitのtestの名前・processの種類・buildの出力の置き場と掃除・testの行の見分け・`init`と`setup-review`の推奨の案に効くことをtestで確かめ続ける（ADR-t2268-1決定8）。
