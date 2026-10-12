---
id: adr-t2268-1
type: adr
title: 言語・道具ごとの知識はruntimeに組み込んだtoolchainのprofileの実装だけが持ち、projectはdagq.tomlでprofileをpathsごとに1つか複数宣言し、計測のコマンドの分類はdagq.tomlの規則・宣言したprofileの既定の規則・runtimeの共通の既定の規則を重ねて当てる（ADR-t614-1決定2・ADR-t1662-3決定16・18・19をamends）
status: accepted
created: 2026-10-12
updated: 2026-10-12
accepted_on: 2026-10-12
amends:
  - adr-t614-1 decision 2
  - adr-t1662-3 decision 16
  - adr-t1662-3 decision 18
  - adr-t1662-3 decision 19
owners:
  - hisamekms
tags:
  - runtime
  - conventions
  - observability
related:
  - adr-t614-1
  - adr-t1662-3
  - adr-t2257-1
  - adr-t639-1
  - adr-t598-1
  - adr-t1942-1
  - adr-t1091-1
  - design-supervisor-lifecycle-source-repository
  - design-measurement
  - design-supervisor-lifecycle-stats
---

# ADR-t2268-1: 言語・道具の知識はtoolchainのprofileが持ち、projectが宣言する

## Context

runtimeは、着地の検証の失敗の読み方（Rustのcompileのerror・nextestとcargo testの失敗の行・rustfmtの差分）と落ちたtestの名前の抜き出し、flakyのやり直しで足すnextestのenv、landing recheckと自動更新のbuildの出力の置き場と掃除、CIの見張りのjob・step・testの名前の読み方、worktime・host_metrics・`stats`・KPIのcargoと`rustc`の読み方を、cargoの前提で直に持つ。
その一部は[ADR-t614-1](2026-09-27-t614-1-dagq-source-only-features-by-one-check.md)決定2の(d)がdagqのソースかの判定で分け、[ADR-t1662-3](2026-10-04-t1662-3-executors-send-telemetry-and-commands-are-classified-by-shape.md)決定16・18・19がコマンドの分類と回数を`dagq.toml`の規則に移すと決めた（未実装）。
Rustでないprojectではこれらが分類できず意味も無く、TypeScriptやPythonを足すにはruntimeのあちこちに分岐を書き足すことになる。

人は2026-10-11（request 95）に、Rustだけ・TSだけ・PythonとTSの複合などさまざまなprojectに対応でき、後から言語を足すのが容易な抽象を求めた。
また同日（ask 728）、計測のコマンドの分類は、profileが既定の規則を持ち、`dagq.toml`にはprojectが足すか上書きする規則だけを書く重ね合わせにし、実際に効く規則を`doctor`で見せると決めた。
この決定は[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)の原則（runtimeは既定を空か拒む側にし、projectの値は`dagq.toml`、hostの値は`host.toml`が与える）の上に立ち、それを変えない。

実装はgoal 189のtask 2269〜2274・2277が受け持ち、コマンドの分類と回数はgoal 110のtask 1674・1675・2278が受け持つ。
箇所ごとの置き場の調べはtask 2268のreceiptにある。

## Decision

1. **抽象。**
   runtimeはtoolchainのprofileのport（trait）を1つ持ち、言語・道具ごとの知識はその実装だけが持つ。
   profileが持つのは、検証の出力からの言語・道具に固有の失敗の分類と落ちたtest・flakyのtestの抜き出し、flakyのやり直しで足すenv、buildの出力とcacheのdirの決め方と掃除（recheckと自動更新のbuild、`[run.env]`の雛形）、CIの見張りのjob・step・testの名前の読み方、host_metricsとKPIのtoolchainの軸の道具の読み方（processの種類と道具の版）、計測のコマンドの分類の既定の規則（ADR-t1662-3決定16と同じ形の規則の列）、推奨の設定（検証コマンドの組み合わせ・reviewのプログラム・宣言の案）である。
2. **置き場の基準。**
   - profile: 同じ道具を使うどのprojectでも同じ知識。
   - `dagq.toml`: projectごとに違う値（流すコマンド・e2eのコマンド・projectのtest targetの名前に依る分類・registryのallowlist・cacheのdirの一覧・projectに固有の検証の出力の印）。
   - `host.toml`: hostごとの値。
   - runtimeの共通の既定: 言語に依らない知識（決定4のhostの分類の汎用の印、待ち・dagq・gitのような道具に依らないコマンドの分類）。
   - runtimeの自前の事情: dagq自身の配布の事情（migrationの振り直し・checkoutからのinstall・source buildの自動更新とそれらのe2eの関門）は、ADR-t614-1のままdagqのソースかの判定に残す。
   この基準は知識の種類ごとの置き場で、個々の関数・module・ファイルの一覧と受け持つtaskの表はこのADRに置かない（[ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)決定3、[ADR-t1942-1](2026-10-07-t1942-1-design-docs-in-four-layers-with-size-budgets.md)決定3）。
3. **宣言と既定。**
   projectは`dagq.toml`で使うprofileを名前つきで1つか複数宣言し、各宣言はprofileの種類と、当てる範囲のpathsを持つ（書かなければrepository全体）。
   宣言しなければどのprofileも働かない。
   `Cargo.toml`などからの推測で有効にせず、推測は`dagq init`の提案にだけ使う。
   知らないprofileの名前は設定の誤りにする。
4. **profileが無くても働くhostの分類。**
   [ADR-t639-1](2026-09-27-t639-1-retry-verification-failures-of-the-host-once.md)のhostの分類（`disk_full`・`killed`・`timeout`）の言語に依らない印（容量の不足のerrorの文言とerrno、signalでの終了とそれを表すshellの終了コード、慣例の時間切れの終了コード、コマンド全体の上限での打ち切り）はprofileに依らずruntimeが持つ。
   宣言の無いrepositoryでも、integrateのhostの失敗のやり直しと保留（ADR-t639-1決定1〜4）は今どおり働く。
   profileとprojectの印は既存の分類（hostの分類を含む）に印を足せるが、分類の集合を広げない（ADR-t639-1決定5）。
5. **計測のコマンドの分類は重ね合わせ（人の判断）。**
   宣言したprofileが既定の規則を持ち、宣言すればそれが効く。
   `dagq.toml`の`[[telemetry.commands]]`には、projectが足すか上書きする規則だけを書く。
   実際に効く規則は、`dagq.toml`の規則を先に、宣言したprofileの既定の規則を宣言の順に次に、runtimeの共通の既定の規則を最後に並べた列で、上から最初の一致を取るADR-t1662-3決定16の当て方は変えない。
   projectの規則が先に一致してprofileの規則を上書きするので、profileの規則を名指して外す口は作らない（同じ形に別の`class`を付けた規則で上書きでき、規則の項目を増やさずに済む。要るとわかったらamendsで足す）。
   `dagq doctor`は実際に効く規則を、各規則の出どころ（`dagq.toml`・profileの宣言の名前・共通）つきで出す。
6. **複数のprofileの当て方。**
   - pathに結びつくもの（差分のpathに当たるprofileの推奨とreviewのプログラムの案、buildの出力とcacheのdirを置く範囲）は、pathsが当たるprofileだけを使う。
   - pathに結びつかない出力（検証とCIの出力、計測のコマンド、hostのprocess）は、宣言した全てのprofileを宣言の順に当て、最初に当たったものを取る。
     検証コマンドは差分のpathに依らずrepositoryを流すので、差分のpathで絞ると分類を落とすためである。
   - 検証の出力に共通の印とprofileの印を当てる順は、今の分類の順を保つ（容量の不足とsignalの印はprofileより先、時間切れの終了コードはprofileのflakyと時間切れの印より後。やり直して通ったtestをflakyと読むため）。
     flakyのやり直しで足すenvは、flakyと分類したprofileのものだけにする。
   - buildの出力とcacheのdirはprofileの宣言ごとに分ける。
7. **推奨は提案だけ。**
   profileの推奨はruntimeの暗黙の既定にしない（ADR-t2257-1決定2・3・5の、`dagq.toml`が与え既定は空の形を保つ）。
   `dagq init`は推測したprofileの宣言の案と推奨の案を案内として出すだけで、`dagq.toml`を書かない。
   built-inのreviewの生成コマンドに渡すのは、ADR-t2259-1決定7がそのコマンドに書かせる範囲（reviewのbuilt-inのtableとreviewのprogramsのtable、`.dagq/review/`の下）に入る推奨（reviewのプログラムの案）だけで、その範囲を広げない。
   検証コマンドの組み合わせ・profileの宣言・containerの材料・registryのhost・workerのcacheのdirの推奨は、initと生成コマンドの出力の案内（`dagq.toml`の案の文）に留め、人が書いてcommitする。
8. **profileは組み込みの実装。**
   新しい言語はprofileの実装1つと登録1行で足す。
   外部のスクリプトや動的な読み込みのprofileは作らない。
   testは偽の2つ目のprofileで、runtimeのほかのコードを変えずに分類・やり直し・cacheの置き場に効くことを確かめる。
9. **移行の不変条件。**
   - 宣言の無いrepositoryで今のRustの振る舞い（dagqのソースかの判定で分けるものはその判定のまま）を暗黙に使うのは、このrepositoryの`dagq.toml`にRustのprofileの宣言が入るまでの間だけで、その間`doctor`が宣言の無いことを警告する。
   - コマンドの分類と回数の旧い分類とdagqのソースかの判定の分岐を外すのは、このrepositoryの`dagq.toml`にprojectの規則が入った後である。
   - 移行の間も、このrepositoryの着地の検証・landing recheck・自動更新・CIの見張り・計測の振る舞いと値は変わらない。
     重ね合わせで規則の順が変わり分類が変わるときは、このrepositoryの`dagq.toml`にprofileの規則を上書きする規則を置いて今の分類を保つ。
   - 新しい設定のkeyをこのrepositoryの`dagq.toml`に足すのは、固定バイナリが読めるようになってからである（ADR-t2257-1）。
10. **範囲の外。**
    migrationの振り直しをprojectのmigrationの道具に広げること、外部のprofileの仕組み、生成コマンドが書き換える範囲をreviewの外に広げることは決めない。

## Amendsの判断

- ADR-t614-1決定2: (d)のうちhostの`rustc`とtoolchainの記録とその分け方を、dagqのソースかの判定から外し、宣言したRustのprofileに移す（ADR-t1662-3決定19が判定に残した部分）。(a)〜(c)・決定1・3と判定そのものは保つ。
- ADR-t1662-3決定16: 規則の出どころを「`dagq.toml`だけ」から決定5の3つの重ね合わせに変える。規則の項目・上から最初の一致・一致しなければ未分類・部分ごとの分類と回数は保つ。
- ADR-t1662-3決定18: profileのある言語では、雛形の代わりにprofileの既定の規則が宣言で効き、`dagq init`は宣言の案を出す。落ちたtestの読み方のアダプタは宣言したprofileのものを使う。profileの無い言語（npm・pnpm・go・pytest・gradle・makeなど）の雛形を同梱して`init`が`[[telemetry.commands]]`の案として提案すること、落ちたtestの名前とpathを設定で有効にすることは保つ（その言語のprofileを足したら雛形はprofileの既定に移す）。
- ADR-t1662-3決定19: このrepositoryの(d)の分類と回数を、Rustのprofileの宣言と`dagq.toml`のprojectの規則（projectのtest targetに依るもの）の重ね合わせに置き換え、hostの`rustc`とtoolchainの記録とその分け方もRustのprofileに移す。宣言も規則も無いrepositoryではcargoを前提にした値を出さないこと、ADR-t614-1の(a)〜(c)・決定1・3を変えないことは保つ。
- ADR-t1662-3決定14・15・17と、shapeを持ち台帳を作るときに分類する形は変えない。
- 置き換えずamendsにする理由: 両ADRとも変えるのは番号付きの決定の一部で、残りの決定は保つ（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）。
- ADR-t2257-1・ADR-t639-1・ADR-t2259-1決定7は変えない。元のADRの本文は書き換えない。

## Alternatives

- **言語ごとの分岐を今の場所に書き足す**: 言語を足すたびにruntimeのあちこちを変え、testで足しやすさを確かめられない。
- **`Cargo.toml`などから自動でprofileを推測して有効にする**: 宣言を忘れたprojectで道具の振る舞いが黙って入り、ADR-t2257-1決定1の既定は拒む側の原則に反する。
- **profileの推奨をruntimeの既定にする**: registryの宛先やcacheが黙って開く。
- **外部のスクリプトでprofileを足す**: 分類のparserを制御側で動かすので、workerが変えられない場所から読む仕組みと安全の設計が要る。まずRustの1つでは要らない。
- **コマンドの分類の規則を`dagq.toml`だけに置き雛形を写させる（ADR-t1662-3の元の形）**: 道具の知識がprojectごとに写され、dagqの更新に追随しない。人が退けた。
- **宣言の無いrepositoryではhostの分類も止める**: ADR-t639-1のhostの失敗のやり直しと保留が失われる。

## Consequences

- 新しい言語はprofileの実装と登録で足し、runtimeのほかのコードは変えない。
- profileを宣言しないprojectでは、言語に固有の分類・やり直しのenv・cacheのdir・toolchainの軸は働かず、hostの分類と共通のコマンドの分類だけが働く。
- 道具の読み方の変更はdagqの更新でprofileごとに届き、projectの`dagq.toml`は足すか上書きする規則だけを持つ。
- どの知識が今どこにあるか（profileに移した部分と、まだ移していない部分の今の場所）は[Source repository](../design/supervisor-lifecycle/source-repository.md)・[計測](../design/measurement.md)・[stats](../design/supervisor-lifecycle/stats.md)が持つ。
