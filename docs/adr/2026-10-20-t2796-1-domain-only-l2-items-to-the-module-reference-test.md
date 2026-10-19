---
id: adr-t2796-1
type: adr
title: L2のうちdomainだけが禁じるanyhow・std::process・std::netの3項目は、clippyの一覧に置けないので、moduleの参照のRustのtestで守り、L2の残りはclippyのままにする（ADR-t2391-1決定1・2、ADR-t2397-1決定2をamends）
status: accepted
created: 2026-10-20
updated: 2026-10-20
accepted_on: 2026-10-20
amends:
  - adr-t2391-1 decision 1
  - adr-t2391-1 decision 2
  - adr-t2397-1 decision 2
owners:
  - hisamekms
tags:
  - architecture
  - testing
related:
  - adr-t2391-1
  - adr-t2397-1
  - adr-0013
  - design-architecture
---

# ADR-t2796-1: L2のうちdomainだけが禁じる3項目は、moduleの参照のRustのtestで守る

## Context

[ADR-t2391-1](2026-10-12-t2391-1-boundary-rules-to-compiler-clippy-and-rust-tests.md)決定1・2は、L2（domainの本番のコードはDB・ファイル・process・network・時計・乱数のID・`anyhow`を参照しない）をclippy（`clippy.toml`の禁止の一覧）に置き、[ADR-t2397-1](2026-10-18-t2397-1-crate-delegated-boundary-rules-kept-in-one-crate-by-a-rust-module-reference-test.md)決定2の表もL2を「clippy（ADR-t2391-1決定2のまま）」とした。

L2をclippyへ移したtask 2393のrun（786966d3）の結論（receiptの(B)）は、時計・乱数のID・DB・ファイル・processの起動はclippyに置けたが、domainだけが禁じる`anyhow`・`std::process`（`std::process::id`を含む）・`std::net`は置けない、である。

- `clippy.toml`の一覧はlintごとにpackageで1つで、lintを有効にした全てのmoduleに同じ一覧が当たる。
- lintはdomainのほかapplicationと起動部分にも有効で、applicationは`anyhow`（`Error`・`Result`・`Context`・`bail!`）と`std::process::id`を、起動部分は`std::process::id`を使う。一覧に入れるとそれらが全て落ちる。

そのため3項目は`scripts/check-layer-deps.sh`のL2に残った。ADR-t2391-1決定5はscriptを移し先ができた規則から削って最後に消すので、3項目にも移し先が要る。

## Decision

L2のうち時計・乱数のID・DB・ファイル・processの起動は、clippyのまま（ADR-t2391-1決定2）にする。

domainだけが禁じる`anyhow`・`std::process`（`std::process::id`を含む）・`std::net`の3項目は、ADR-t2397-1決定1のmoduleの参照のRustのtestに置く。

- 同じparserと型付きの表を使い、別の仕組みを作らない。testは`use`とpathを、crateの根からのmoduleのpathか、外のcrateのpathに解く（別名の`use`・`extern crate`の別名・macroのpathも辿る）。
- 当たるのは`src/domain`の本番のコード（`test=false`のビルドに残りうるコード）だけで、`#[cfg(test)]`の中、applicationと起動部分は数えない。
- 例外と今ある違反は、L1・L3などと同じ表に理由と行き先付きで置く。表に無い参照と、参照が消えた項目で落ちる（ADR-t2391-1決定3の扱いのまま）。今の違反は無い。
- 移した同じ変更で、scriptからL2を外す（ADR-t2391-1決定5の順）。

### amendsの範囲

- **ADR-t2391-1決定1**: 規則ごとの移し先の表のL2の行を、clippyと、3項目は決定1のtest、に変える。他の行は保つ。
- **ADR-t2391-1決定2**: 禁止するitemの一覧を`clippy.toml`に置く範囲から、domainだけが禁じる3項目を外す。一覧の置き場とlintの有効にし方は保つ。
- **ADR-t2397-1決定2**: 規則ごとの境界の表のL2の行を、clippy（ADR-t2391-1決定2のまま）と、3項目は決定1のtest、に変える。他の行は保つ。

## Alternatives

- **clippyの一覧に入れ、applicationと起動部分の使用を個々の`#[expect]`で許す**: applicationの`anyhow`の使用は広く、`#[expect]`が大量に要る。規則に反しない使用を例外として並べることになり、ADR-t2391-1決定2の「許す場所は理由付きの個々のitemだけ」の意味が薄れる。
- **scriptに残す**: ADR-t2391-1決定5は移し先ができた規則からscriptを削って最後に消すので、移し先の無い規則を残すとscriptを退役できない。字句で読むscriptは、別名やmacroの中のpathも見落としうる。
- **domainを別のcrateに切り出し、依存に`anyhow`を入れない**: ADR-t2397-1がgoal 121の測定から退けた。`std::process`・`std::net`はstdの一部で、crateの依存では禁じられない。

## Consequences

- L2の検査は、clippyとmoduleの参照のtestの2か所に分かれる。どちらも`cargo clippy`と`cargo test`の着地の検証で毎回流れる。
- `scripts/check-layer-deps.sh`にはC8だけが残り、ADR-t2397-1決定4の2段目でtestへ移る。
- domainだけが禁じる外のcrateやstdのmoduleを足すときは、testの禁止の一覧に足す。
