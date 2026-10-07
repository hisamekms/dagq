---
id: adr-index
type: design
title: Architecture decision records
status: current
created: 2026-09-21
updated: 2026-10-07
last_verified: 2026-09-30
tags:
  - architecture
  - documentation
---

# Architecture decision records

ADRは、将来の実装や運用に大きな影響を与える決定の理由を残す。ADRの書き方・ID・置き換えとamends・append-only・索引の生成の規則は[文書の規則](../development/documents.md)の「ADR」「ADRのID」、欄・状態・注記の書式は[frontmatter仕様](../frontmatter.md)の「ADR fields」が持つ。

## Status

状態の意味は[frontmatter仕様](../frontmatter.md)の「ADR fields」の表。新しいADRの作り方は[文書の規則](../development/documents.md)の「ADR」。

## 索引

索引は`docs/adr/INDEX.md`で、各ADRのfrontmatterから生成し、commitしない（[ADR-t1967-1](2026-10-07-t1967-1-adr-index-generated-from-frontmatter.md)）。
無ければ`sh scripts/adr-index.sh`で作り、`--force`で作り直す（ADRを足した・状態を変えた後も）。
有効なADR（`status: accepted`）と、置き換え・廃止されたADR（`superseded` / `deprecated`）と後継の表を持つ。
ADRを足す・状態を変える変更はこのREADME.mdにも索引にも行を足さず、frontmatterの欄だけを直す。

cloneの後に1回`git config core.hooksPath .githooks`を打つと、`.githooks/post-checkout`がcheckoutと`git worktree add`のたびに、`.githooks/post-merge`がmergeのたびに索引を作り直す。

## 地図

0001〜0034の決定の判定・後継ADR・今の姿を持つdesignの参照先は[ADRの対応表](../plans/adr-inventory.md)にある。
0001〜0034の棚卸しの旧組A〜CはADR-0052〜0054への置き換え済みの記録で、組D〜Jの統合ADR計画は取り消された（[対応表](../plans/adr-inventory.md#統合adrの組過去の記録)）。
ADR-t598-1決定4に従い、今の姿をまとめる統合ADRは作らない。

goal 82（goal 38の段(1)〜(3)）のADRはtask 1233が書いた5本で、決定の置き場所は次のとおり。制御側と実行側の分け方・queue service・ユースケース単位のAPIとservice側の認可・unix socket・broker・クライアントモード・段の順と置き場所はADR-t1233-1、serviceの起動・停止の責任・落ちたときの知らせ方・brokerより前のprincipalの認証（token）はADR-t1233-4、読み取りのユースケースとroleごとの読める範囲・workerの読める範囲・Codexのsandboxからの到達はADR-t1233-5、e2eをreviewのpassの後のhostの工程に移すことはADR-t1233-2、このrepositoryを先にLinuxで通すことはADR-t1233-3にある。
