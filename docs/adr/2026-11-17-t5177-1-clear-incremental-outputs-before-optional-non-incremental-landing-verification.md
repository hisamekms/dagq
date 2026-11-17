---
id: adr-t5177-1
type: adr
title: 着地の検証だけincrementalを消して使わない選択をrepositoryに持たせる
status: accepted
created: 2026-11-17
updated: 2026-11-17
accepted_on: 2026-11-17
amends: [adr-0049 decision 6]
owners:
  - hisamekms
tags:
  - runtime
  - verification
related:
  - adr-0049
  - adr-t1925-1
  - adr-t2268-1
  - adr-t5108-1
---

# ADR-t5177-1: 着地の検証だけincrementalを消して使わない選択をrepositoryに持たせる

## Context

走っているrunのincrementalが約9 GiBを占め、disk_spaceのclaimの保留を生んでいる（goal 286）。
[buildの出力の測定](../plans/run-build-output-size-spike.md)では、incrementalを使わない案はtargetを約79%減らしたが、workerの再buildが22秒から93秒になった。
workerの繰り返しのbuildは速いままにし、着地の検証だけで減らしたい。
envだけを変えると古いincrementalに非incrementalの成果物が加わり、容量が増えうる。
request 347の人の決定で、追加のSpikeでなく本番で切り替えて測り、戻せる形にする。

## Decision

1. ADR-0049決定6のうち、incrementalを変えない決定を着地の検証についてだけ改める。
   repositoryがcommitする設定で、着地の検証だけincrementalを使わずに流すことを選べる。
   選んだときはreviewのプログラムとrebaseの後、最初の検証の前にそのrunのincrementalの出力を一度消すことと組にする。
   compilerがlockを持つsessionは残し、削除の失敗を記録しても検証は流す。
   登録のままのコマンドと置き換えたコマンド、環境の失敗とflakyのやり直しに同じenvを渡す。
2. `[run.env]`とworkerのbuildでは`CARGO_INCREMENTAL`を変えない。
   landing recheck・already_satisfiedの検証・reviewのbuildも変えない。
   ADR-0049決定6の`CARGO_TARGET_DIR`をrun間で共有しない決定を保つ。
   ADR-t1925-1決定4の置き換えと、置き換えたコマンドにだけ材料のenvを渡す境界を保つ。
   道具の知識はprofileが持つADR-t2268-1を保ち、削除先・lock・非incrementalのenvをprofileに委ねる。
   ADR-t5108-1決定3を保ち、運用のlocalの層で着地の検証を変えることは許さない。
3. このrepositoryは後続の設定変更で本番をonにし、印の前後を比べる。
   着地の検証後のtargetの最大GiB、着地の検証の時間、失敗後のworkerの手直しの最初のbuildの時間、1時間あたりの着地数を測る。
   request 347の人の決定どおり、着地の検証の時間が25%超増えるか、手直しの最初のbuildが1.5倍超なら設定をoffに戻す。
   戻した後のbuildはincrementalを作り直し、workerのbuildの設定変更は要らない。

## Alternatives

`[run.env]`で全てのbuildのincrementalを切る案はworkerの繰り返しのbuildの代償が大きい。
出力を消さず着地のenvだけ変える案は、空き容量を減らしかねない。
run間のtarget共有は小さい依存crateの出力しか減らさず、既存の共有しない決定を覆さない。

## Consequences

着地の前にincrementalを空けるので、走っているrunの占める容量を減らせる。
着地の検証が落ちた後、workerの手直しの最初のbuildはincrementalを失った状態から始まる。
[sccache 0.19.0のRustの鍵の生成](https://github.com/mozilla/sccache/blob/v0.19.0/src/compiler/rust.rs#L1753-L1789)を確認した。
`CARGO_`で始まるenvの名と値をhashに含め、除外の一覧に`CARGO_INCREMENTAL`は無い。
そのため着地の検証でcompileするcrateの鍵はworkerのbuildと分かれうる。
依存crateのcacheの正しさやcoverageの関門を弱める変更はしないが、hitと時間の代償は本番の測定に含める。
