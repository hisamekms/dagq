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

**実装状況**: portと登録とRustのprofile、`[toolchains.<name>]`の宣言、検証の出力の読み方（失敗の分類・落ちたtestとflakyのtestの抜き出し・flakyのやり直しのenv）はある。
buildの出力とcacheの置き場と掃除・CIの見張りの読み方・計測の道具の読み方・推奨の設定はまだportに無く、今の場所は[Source repository](supervisor-lifecycle/source-repository.md)の「言語・道具の知識の今の場所」にある。

## portの責務

- portは`domain::toolchain::ToolchainProfile`の1つのtraitで、1つの実装が1つの言語の道具の知識を持つ。
  各methodの意味と、runtimeが呼ぶ順はそのdoc commentが持つ。
- 今のmethodは検証のコマンドの出力を読むものだけで、道具の印から失敗を既存の分類に当てること、落ちたtestと流し直しで通ったtestの名前を読むこと、flakyのやり直しで足すenvを返すことである。
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
- 検証の出力のようにpathに結びつかない出力は、宣言した全てのprofileを宣言の順に当て、各段で最初に当たったものを取る（ADR-t2268-1決定6）。
  検証のコマンドは差分のpathに依らずrepository全体を流すので、pathsで絞ると分類を落とすためである。
  pathsはpathに結びつくもの（buildの出力とcacheの置き場、推奨の案）のためにあり、今は`doctor`と`status`に当たる範囲として出るだけである。
- 宣言したprofileがどれも読まない出力は、共通の印に当たらなければ`unknown`で、落ちたtestの名前は無く、flakyのやり直しのenvも足さない。
- 宣言が無いrepositoryでは、移行の間だけRustのprofileが暗黙に当たり（`Toolchains::undeclared`）、`doctor`と`status`が警告する（ADR-t2268-1決定9）。
  profileもprojectの印も宣言しない間は、`within`の行の印も同じく暗黙に当たり（`verify_marks::implied_within`）、どちらかを宣言すると宣言したものだけが効く。
  `Cargo.toml`などからprofileを推測して有効にすることはしない。
- 読むのはqueueが束縛されたmain checkoutの作業ファイルの`dagq.toml`で、`integrate`とlanding recheckが検証のたびに読む。
  workerのtranscriptとCodexのturnの計測は、まだ宣言を読まず暗黙のRustのprofileで落ちたtestの名前を読む。
- 組み込みに無いprofileの名前は設定の誤りで、`integrate`は検証の前に着地処理のエラーにし、`doctor`と`status`は`toolchains`に`error`を出す（`domain::toolchain::Toolchains::view`）。
- 旧いバイナリは`[toolchains.<name>]`を知らない表として拒むので、repositoryが宣言を足すのは固定バイナリが読めるようになってからにする（[Run environment](supervisor-lifecycle/run-environment.md)）。

## 新しいprofileの足し方

- `ToolchainProfile`を実装する型を1つ書き、`domain::toolchain::BUILTIN`の登録に1行足す。
  runtimeのほかのコードは変えない（ADR-t2268-1決定8）。
- 印の一覧と道具の版、やり直しのenvの意味は、その実装の定義のそばのdoc commentに書き、designに列挙しない。
- 外部のスクリプトや動的な読み込みでprofileを足す仕組みは無い。

## テスト

- 偽の2つ目のprofileを実装と登録だけで足し、分類・落ちたtestの抜き出し・やり直しのenvに効くことをtestで確かめ続ける（ADR-t2268-1決定8）。
