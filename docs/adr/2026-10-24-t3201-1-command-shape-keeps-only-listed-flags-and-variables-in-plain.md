---
id: adr-t3201-1
type: adr
title: コマンドの形は一覧にあるプログラムのflagの名前も一覧が知るoptionの名前だけを平文で持ち、代入の名前も共通の一覧と宣言したprofileの一覧にあるものだけを平文で持つ（ADR-t1673-1決定9・ADR-t1662-3決定15をamends）
status: accepted
created: 2026-10-24
updated: 2026-10-24
accepted_on: 2026-10-24
amends:
  - adr-t1673-1 decision 9
  - adr-t1662-3 decision 15
owners:
  - hisamekms
tags:
  - runtime
  - security
  - observability
related:
  - adr-t1673-1
  - adr-t1662-3
  - adr-t2268-1
  - design-measurement-commands
  - design-toolchain-profiles
---

# ADR-t3201-1: コマンドの形はflagと代入の名前も一覧にあるものだけを平文で持つ（ADR-t1673-1決定9・ADR-t1662-3決定15をamends）

## Context

[ADR-t1673-1](2026-10-21-t1673-1-command-shape-keeps-only-known-subcommands-in-plain.md)は、人の判断（ask 1055）で、コマンドの形が平文に残すのを一覧にあるプログラム名とその既知のサブコマンドに限った。字句の解析が誤っても、一覧に無い語は平文にならないようにするためである。

一方で決定9は、一覧にあるプログラムのflagの名前（`-`で始まる語）と変数の代入の名前（`NAME=value`の`NAME`）を[ADR-t1662-3](2026-10-04-t1662-3-executors-send-telemetry-and-commands-are-classified-by-shape.md)決定15のまま形で決めて平文に残した。字句の解析が引用の中身を誤ってflagや代入の名前の位置に置くと、その語は平文でstepに入る。決定6は字句を読み切れないコマンドを全てdigestにするが、引用が釣り合ったまま誤る形はそこに倒れない。flagと代入の名前は形で決める最後の平文で、ask 1055が塞いだ穴と同じ理由の穴が残る。人はask 1093でもtelemetryに平文を残さない判断を重ねた。

## Decision

1. **flagの名前は一覧が知るoptionだけ平文。** 一覧にあるプログラムのflagは、そのプログラムの一覧が知るoption（値を取らないoptionと、値を取る既知のoption）の名前のときだけ名前を平文で持つ。知らない名前はflagの形のまま名前をdigestにする。`--name=value`のvalueと短いflagに付いた値は今どおりdigestにする。
2. **値を取るかの読み方は変えない。** 値を取る既知のoptionの一覧は名前を平文にするかだけに使う。どの語がoptionの値になりうるかは、ADR-t1673-1決定7のまま値を取らないoptionの一覧で決める。
3. **代入の名前は一覧にあるものだけ平文。** 部分が代入する変数の名前（`NAME=value`と`for NAME in`）は、道具に依らない共通の一覧と、宣言したprofileの一覧にあるときだけ平文で持ち、ほかはdigestにする。一覧は名前と接頭辞で書ける。値はどの名前でも持たない。
4. **一覧の置き場はADR-t1673-1決定3のまま。** 共通の道具のoptionと共通の変数の一覧はshapeを作る関数のそばに置き、どのrepositoryでも効く。言語・道具ごとのoptionと変数（cargo・rustupのoption、Rustの道具の環境変数など）はその道具のtoolchainのprofileが持ち、宣言したときだけ効く。具体的な名前は定義のそばのdoc commentとコードが持つ。
5. **分類はdigestにしたflagの名前とも照合する。** 分類の規則の`flags`・`none_of`・`flag_values`・`takes_value`のflagの名前のliteralは、ADR-t1673-1決定8の語と同じく、平文のflagの名前ともdigestにしたflagの名前とも照合する。一覧に無いflagを読む規則も、一覧を足さずに書けるようにするため。
6. ADR-t1673-1決定9のうち`--`とリダイレクトの扱い、ADR-t1662-3決定15のほかの項目、決定14・16〜19は変えない。

## Alternatives

- **flagと代入の名前を形で決めたままにする（決定9のまま）**: 釣り合った誤りで引用の中身が平文で残り、ask 1055の「一覧に無い語は平文にならない」を満たさない。
- **flagと代入の名前を全てdigestにする**: 平文は残らないが、`cargo test --locked`のような道具のflagも読めなくなり、stepを読む人と`doctor`の出力から形が分からなくなる。分類はdigestと照合できるが、一覧が知る名前を伏せる理由は無い。
- **値を取るoptionの一覧で値の読み方も決める**: 一覧に無い値を取るoptionの`-`で始まる値が平文で残る（ADR-t1673-1の退けた案と同じ）。名前を平文にするかだけに使えば、一覧の抜けは名前を失うだけになる。
- **変数の一覧をrepositoryの設定にする**: 設定の無いrepositoryで既定が安全にならない（ADR-t1673-1の退けた案と同じ）。

## Consequences

- 平文に残るのは、一覧にあるプログラム名とその既知のサブコマンド、そのプログラムの一覧が知るoptionの名前、shellの予約語、一覧にある変数の名前だけになる。
- 一覧に無いoptionと変数は、一覧を足すまで名前を平文で読めない。分類は決定5でdigestと照合するので変わらない。
- profileを宣言しないrepositoryでは、Rustの道具の変数（`RUST_LOG`など）の名前も平文にならない。
- `for`の変数の名前も一覧に無ければdigestになる。
- 今の姿は`domain::command_shape`のmodule docと[計測のコマンドの形と分類](../design/measurement-commands.md)、profileが返すものは[Toolchain profiles](../design/toolchain-profiles.md)の「portの責務」が持つ。
