---
id: adr-t1453-2
type: adr
title: AGENTS.mdをこのrepositoryの開発の短い案内に絞り、規則・手順・設計・経緯・測定・設定値・reviewのsubagent定義の正本を1か所ずつに決め、移すときは同じ変更で参照に置き換え、AGENTS.mdのbyteの上限をCIで検査する
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
owners:
  - hisamekms
tags:
  - documentation
  - conventions
related:
  - adr-t598-1
  - adr-t1453-1
  - docs-index
  - docs-frontmatter
---

# ADR-t1453-2: AGENTS.mdをこのrepositoryの開発の短い案内に絞り、規則・手順・設計・経緯・測定・設定値・reviewのsubagent定義の正本を1か所ずつに決め、移すときは同じ変更で参照に置き換え、AGENTS.mdのbyteの上限をCIで検査する

## Context

AGENTS.mdは約105KB（2026-10-03のmainで105,197 byte）になり、dagqの共通の操作手順（pluginと重複）、実装の設計（`docs/design/`と重複）、判断の経緯（task番号・測定値・日付）、今の設定値（`dagq.toml`のコメントと重複）、このrepositoryの開発の規則が混ざっている。全てのsessionがこれを読み、同じ規則が複数の場所にあってずれる。pluginの`reference/scope.md`にもこのrepository固有のRustの検証コマンドや測定の経緯が入っている。人は2026-10-03にgoal 94で、AGENTS.mdはこのrepository自身の開発に要る注意だけを持ち、runtimeにこのrepository固有の規則を埋め込まないと決めた。

## Decision

1. **受け持ち。** 規則・手順・事実の本文は次の表の1か所だけに置き、ほかの場所は参照する。

   | 置き場 | 持つもの |
   | --- | --- |
   | AGENTS.md | 概要、誰が・いつ・どの文書を読むかの案内、本番queueと開発環境の境界、作業の開始に要る短い制約（固定バイナリを使う、開発中のバイナリで本番queueを変えないなど）、検証と文書の規則への参照 |
   | plugin（`plugins/claude-dagq`） | dagqの共通の操作手順。利用するrepositoryの規則を参照する汎用の手順で、このrepository固有の規則・値・経緯を持たない |
   | 開発文書（`docs/development/`） | このrepositoryの開発の今の規則: plannerのverify・paths・evidence・changeの選び方、workerのtestの範囲とstress、testの制約、文書の規則など |
   | `docs/design/` | 実装の今の姿 |
   | ADR | 判断の理由と経緯（過去のtask番号・日付・退けた案） |
   | `docs/plans/` | 測定と計画 |
   | `dagq.toml` / `host.toml` | 今の設定値（値の理由はコメントから開発文書・ADR・plansを指す） |
   | reviewのsubagent定義 | 検査項目と、開発文書・designへの参照だけ（[ADR-t1453-1](2026-10-03-t1453-1-review-subagents-named-by-path-run-inside-the-review-job.md)） |

2. **開発文書の置き場と型。** 開発文書は`docs/development/`に置き、frontmatterの`type`を新しい`development`にする（statusは`current` / `deprecated`）。ADRと違い今の規則を持ち、書き換えてよい。designは実装の今の姿で、開発の規則とは分ける。型は[frontmatter仕様](../frontmatter.md)と[文書の案内](../README.md)に同じ変更で足した。
3. **読む量を絞る案内。** AGENTS.mdは役割（worker・planner・plan review・review・inbox）と変更の範囲（runtime・docs・pluginなど）ごとに、読む開発文書を名指す。移した先の全ファイルを全員に読ませない。plannerのverifyの選び方やworkerのtestの範囲のように着手の前に要る規則は、reviewに移すだけで済ませず、読む案内に載せる。
4. **移し方。** 規則を移すときは、同じ変更で元の場所（AGENTS.md・plugin）を参照に置き換え、複製の期間を作らない。新しい読む経路と必要な検査が使えるようになる前に既存の指示を消さず、禁止事項と例外を落とさないことを移動の前後で照合する。
5. **runtimeに固有の規則を入れない。** runtimeは設定（`dagq.toml`）を解釈して実行と結果の契約を強制する汎用の仕組みだけを持ち、このrepository固有の規則・検証コマンド・文書の対応はrepositoryが開発文書・設定・subagent定義で供給する。
6. **AGENTS.mdの大きさの上限。** AGENTS.mdの大きさを、行数でなくbyteで、scriptとCIが機械的に検査する。上限の値は、AGENTS.mdを短くした後のtaskが実際の大きさから決め、値と決め方を開発文書に記録する。このADRは値を決めない（[ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)決定3）。

## Amendsの判断

ADR-t598-1はamendsしない。決定4（今の姿は`docs/design/`、理由はADR）は実装の今の姿の置き場を決めたもので、開発の規則という別の種類の置き場を足しても実装の今の姿の正本は変わらない。決定3（ADRに数値や名前を書かない）にはこのADRも従う。文書の種類の一覧はADRでなく文書の案内が持つので、そこに足した。

## Alternatives

- **開発の規則を`docs/design/`に置く**: designは実装の今の姿を持ち`last_verified`をコードと照らす。開発の規則は実装でなく、照らす相手が違い、読む人も違う。
- **開発の規則をpluginのreferenceに置く**: pluginは利用するrepositoryに配布する汎用の手順で、このrepository固有の規則を持つと他のrepositoryに誤って効く。
- **subagent定義に規則の本文を持たせる**: 着手の前に読む人（plannerとworker）が定義を読まず、正本が2か所になる。
- **AGENTS.mdを行数で制限する**: 1行が長い日本語の文書では行数が大きさを表さない。
- **ADRに上限の数値を書く**: 数値はdesignかdevelopmentの持ち物で、変えるたびにADRが要る。

## Consequences

- AGENTS.mdの棚卸し、開発文書への移動、subagent定義と有効化、AGENTS.mdの組み直しとbyteの上限の検査、pluginの汎用化、照合のtaskが続く（goal 94の順序）。
- 移すまでの間、規則の正本は今のAGENTS.mdのままで、移した規則から順に開発文書が正本になる。
- `docs/development/`の文書はADRと違い書き換えるので、経緯はADRとplansに分けて残す。
