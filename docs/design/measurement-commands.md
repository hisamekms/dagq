---
id: design-measurement-commands
type: design
title: 計測のコマンドの形と分類（shapeの記録と詳しさ・規則の重ね合わせ・台帳と作業の内訳の分類）
status: current
created: 2026-10-12
scope: runtime
related:
  - design-measurement
  - adr-t1662-3
  - adr-t2268-1
  - adr-t3201-1
  - adr-t614-1
  - design-toolchain-profiles
---

# 計測のコマンドの形と分類

[計測](measurement.md)の一部で、stepが持つコマンドの形とその詳しさ、台帳と作業の内訳がコマンドに付ける分類を書く。

（ADR-t1662-3決定14〜19、[ADR-t2268-1](../adr/2026-10-12-t2268-1-toolchain-knowledge-behind-declared-profiles.md)決定5・(4)・(8)）

## shapeと詳しさ

- shapeは`domain::command_shape`の副作用の無い関数が作り、executorがstepに付ける（語の扱いと上限はそのmodule doc）。
  shellの`-c`のscript（Codexのコマンド）はその中身の部分として持つ。
- shapeは部分（パイプ・`&&`・`||`・`;`で分けたもの）ごとに先頭の予約語・代入の名前・プログラム・引数を持つ。
  平文で持つのは決まった一覧にあるプログラム名とそのサブコマンドだけで、一覧に無いプログラムは名前もその後の語もdigestにする（[ADR-t1673-1](../adr/2026-10-21-t1673-1-command-shape-keeps-only-known-subcommands-in-plain.md)）。
  一覧にあるプログラムのflagの名前は、その一覧が知るoptionのときだけ平文で、ほかはflagの形のまま名前をdigestにする。
  代入の名前は、共通の一覧と宣言したprofileの一覧にあるときだけ平文で、ほかはdigestにする（[ADR-t3201-1](../adr/2026-10-24-t3201-1-command-shape-keeps-only-listed-flags-and-variables-in-plain.md)）。
  digestはqueueごとの秘密の鍵によるkeyed digestで、同じqueueの中では同じ語が同じdigestになる。
  鍵の無いexecutorはコマンドの形を送らない（作り方と鍵の扱いはADR-t1673-1決定10と`DigestKey`のdoc comment）。
- stepが持つ詳しさは`dagq.toml`の`[telemetry] command_detail`が決め、executorが`Subcommands`に載せてshapeとstepに当てる（`CommandDetail`）。
  変数の値はどの詳しさでも持たない。
  書式・値の意味・既定値は`domain::command_rules::TelemetrySettings`のdoc commentが持つ。
- workerのtranscriptとCodexのturnを読むときは、落ちたtestの名前を既定で読まず保存もしない。
  `[telemetry] failed_tests`を有効にしたときだけ宣言したprofileの読み方で読み、作業の内訳に渡す（`command_rules::reads_failed_tests`）。
  profileを宣言していなければ読まない。

## 分類の流れ

- stepには分類を書かない。
  分類は台帳を作るとき（ビュー）にその時に効く規則で決め、規則の版が変われば新着の入力が無くても確定したrunも作り直す（`ledger::builder::Builder::mark_rules`）。
- 効く規則（`effective_rules`）は、`dagq.toml`の`[[telemetry.commands]]`（pjが足すか上書きする規則）、宣言したtoolchainのprofileの既定の規則を宣言の順、runtimeの共通の規則（待ち・dagq・git）の順に並べた列で、部分ごとに上から最初に一致した規則の分類を付ける。
  コマンドの分類は、部分の分類のうち規則の並びで先のもの。
  台帳はコマンドの分類と並べて部分ごとの分類も持つ（`ledger::steps::ClassedStep`）。
  どれにも一致しなければ未分類。
- 宣言も`[[telemetry.commands]]`も無いrepositoryは共通の規則だけで分類し、言語の道具のコマンドは未分類になる（宣言しなければどのprofileも当てない。ADR-t2268-1決定3）。
- 規則の項目と一致の意味は`CommandRule`と`CommandClassifier`のdoc commentが持つ。
  literalは平文の語ともそのdigestとも一致するので、一覧に無いプログラムと一覧に無いflagの規則も書ける。
  `program`と`redacted`の詳しさのstepには値と語が無いので、それを見る規則は、無いことを求める規則も含めて一致しない。
- runtimeは分類の名前に意味を持たせない。
  例外は作業の内訳の`integrate`と重なる検証の数で、`CHECK_CLASSES`の名前の分類から数える。
- `dagq doctor`の`command_rules`が効く規則の列を出どころ（`dagq.toml`・`profile:<名前>`・`common`）つきで出す。

## profileの規則とpjの規則の分担

- profileの既定の規則は道具の知識だけを持つ（`ToolchainProfile::command_rules`。Rustのprofileはcargoのbuild・test・fmt・llvm-covと、targetもfilterも無い`cargo test`の`full_test`）。
  profileを宣言すれば効き、`[[telemetry.commands]]`には書かない。
- pjのtest targetの名前に依る分類（どのtargetがe2eか、どのtargetが全体に近いか）はpjの規則として`dagq.toml`に書く。
- profileの無い道具（npm/pnpm・go・pytest・gradle・make）の規則は雛形としてバイナリが持ち（`command_rules::templates`）、`dagq init`がrepositoryのファイルから当てはまるものを`telemetry_commands`の案として出す。
  案は書き込まない。
  その言語のprofileを足したら、雛形はprofileの既定の規則に移す。

## 台帳と作業の内訳

- 台帳はrunの行を畳むときにcommandのstepを分類し、区間ごとの分類と上の段の集計の時間を規則の版（`EffectiveRules::version`）つきで持つ（`StepFold::classify`）。
  規則の版が変われば、storeが持つstepからその行の分類を作り直す。
  保持で消えたstepは作り直しに入らない。
  規則はsupervisorの台帳を作る係がpassごとに`dagq.toml`とqueueの鍵から読む（`LedgerPort::classifier`）。
- runの作業の内訳（`session_closed`の`work`と`worktime.jsonl`）は、どのrepositoryも台帳と同じ効く規則（`domain::command_rules::work_rules`）で分類し、dagqのソースかの判定では分けない。
  `integrate`と重なる検証の数は効く規則の分類から作り、`CHECK_CLASSES`の名前の分類を出す規則が無ければ出さない。
  宣言も規則も無いrepositoryは共通の規則だけで分類し、cargoを前提にした分類も数も出さない。
