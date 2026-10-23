---
id: design-measurement-commands
type: design
title: 計測のコマンドの形と分類（shapeの記録と台帳の分類の予定）
status: draft
created: 2026-10-12
scope: runtime
related:
  - design-measurement
  - adr-t1662-3
---

# 計測のコマンドの形と分類

> **一部だけ実装済み**: [計測](measurement.md)の一部で、shapeは実装済みで今の姿を書く。
> `command_detail`と分類はまだ`src/`に無い予定。

（ADR-t1662-3決定14〜19、[ADR-t2268-1](../adr/2026-10-12-t2268-1-toolchain-knowledge-behind-declared-profiles.md)決定5）

- `[telemetry] command_detail = "program" | "shape" | "redacted"`（既定`shape`）。
- shapeは`domain::command_shape`の副作用の無い関数が作り、executorがstepに付ける（語の扱いと上限はそのmodule doc）。
  shellの`-c`のscript（Codexのコマンド）はその中身の部分として持つ。
- shapeは部分（パイプ・`&&`・`||`・`;`で分けたもの）ごとに`lead`（先頭の予約語）・`assigns`（代入の名前）・`program`・`args`（`{flag: 名前, value?: digest}`・`{sub: 語}`・`{word: digest}`・`{plus: digest}`・`{after_dashdash: 数}`）。
  平文で持つのは決まった一覧にあるプログラム名とそのサブコマンドとflagの名前だけで、一覧に無いプログラムは名前もその後の語もdigestにする（[ADR-t1673-1](../adr/2026-10-21-t1673-1-command-shape-keeps-only-known-subcommands-in-plain.md)）。
  digestはqueueごとの秘密の鍵によるkeyed digestで、同じqueueの中では同じ語が同じdigestになる。
  鍵の無いexecutorはコマンドの形を送らない（作り方と鍵の扱いはADR-t1673-1決定10と`DigestKey`のdoc comment）。
- 分類は台帳を作るときに、`dagq.toml`の`[[telemetry.commands]]`・宣言したprofileの既定の規則・runtimeの共通の規則（待ち・dagq・git）の順の列を上から当てる。
  `dagq doctor`が効く列を出どころつきで出す。
- **実装状況**: `command_detail`と分類は未実装で、分類は記録の時に`domain::worktime`が付ける（[Source repository](supervisor-lifecycle/source-repository.md#cargo専用の計測)）。
- 今の分類を規則で表す例（1〜3つ目はこのrepositoryの`dagq.toml`で2つ目は今の順を保つ上書き、ほかはRustのprofile）:

```toml
[[telemetry.commands]]
class = "e2e"
program = "cargo"
flag_values = { "--test" = ["e2e"] }
takes_value = ["--test"]

[[telemetry.commands]]
class = "llvm_cov"
program = "cargo"
sub = ["llvm-cov"]

[[telemetry.commands]]
class = "full_test"
group = "test"
program = "cargo"
sub = ["test"]
flag_values = { "--test" = ["it"] }
none_of = ["--lib", "--bin", "--bins", "--doc", "--example", "--package", "-p", "-E"]
takes_value = ["--test"]
positional = "none"

[[telemetry.commands]]
class = "full_test"
group = "test"
program = "cargo"
sub = ["test"]
none_of = ["--test", "--lib", "--bin", "--bins", "--doc", "--example", "--package", "-p", "-E"]
positional = "none"

[[telemetry.commands]]
class = "test"
program = "cargo"
sub = ["test"]
```

`cargo test --locked --test it`は3つ目の規則で`full_test`、`cargo test --locked --test plugin`は`plugin`が`["it"]`に無いので3つ目と4つ目（`--test`が在る）に一致せず5つ目で`test`になる。
`positional`が除くのは規則の`sub`が一致した語だけなので、`cargo test runtime`（`runtime`は既知のサブコマンドでないのでdigestで残る。[ADR-t1673-1](../adr/2026-10-21-t1673-1-command-shape-keeps-only-known-subcommands-in-plain.md)）は3つ目と4つ目に一致せず`test`になる。
台帳は部分ごとの分類も持ち、`integrate`と重なる検証の数（`full_tests`・`llvm_cov_runs`）は部分の分類から数える（`cargo llvm-cov … && cargo test --locked`は両方に数える）。
`cargo nextest`とbuild・clippy・fmtも同じ形の規則でRustのprofileに入る。
今と分け方が違う形: `--test e2e --test it`（今はe2e、この規則では`test`）と`nextest run --test it`の`full_test`（規則を足せば表せる）。
差は段2の並走比較で説明する。
落ちたtestの名前とpathは設定で有効にし、読み方は宣言したprofileのもの。
profileの無い言語の雛形は同梱し、`dagq init`が案として出す。
