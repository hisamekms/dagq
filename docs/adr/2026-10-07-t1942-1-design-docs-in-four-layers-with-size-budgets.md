---
id: adr-t1942-1
type: adr
title: 文書を概念・地図・doc commentの詳細・ADRとgitの経緯の4層に分け、designには概念と地図だけを書き、細かい事実はコードのdoc comment、経緯はADRとgitに置き、designを1文1行と文書ごとの大きさの予算で保ち、検査を増やす方向と減らす方向の両方にする（ADR-t598-1決定3・4をamends）
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
amends:
  - adr-t598-1 decision 3
  - adr-t598-1 decision 4
owners:
  - hisamekms
tags:
  - documentation
  - conventions
related:
  - adr-t598-1
  - adr-t1091-1
  - adr-t1453-2
  - adr-t1854-1
  - adr-t1942-2
  - development-documents
---

# ADR-t1942-1: 文書を4層に分け、designには概念と地図だけを書き、1文1行と大きさの予算で保つ（ADR-t598-1決定3・4をamends）

## Context

2026-10-06の人の依頼（request 44、goal 159）。
docsの役割は、AIがよく参照する情報をコードを読まずに分かるよう要約してcontextを節約することと、人の理解を助けることである。
`docs/design/`は今その逆になっている。

- 2026-10-07の時点で97文書・約3.0MB、30 KiB超が29文書、1行が2,000 byte超の行が197行ある。
- 上位4文書（persistence・domain-model・supervisor-lifecycle/stats・provider-lifecycle）は9/22から10倍に伸び、本文の48〜67%がバッククォートの識別子、1文書あたりtask番号が54〜153回出る。
- 直近14日のworkerとreviewの会話2,482件で、4文書を丸ごと読んだのは0回で、grepの結果は1行が長く段落ごと返る（p90で6.9K文字）。
- 9/29以降の着地438件の70%が`docs/design/`を変え、`conflict_hotspots`は全て`docs/design/`と`docs/adr/README.md`だった。

原因は4つある。
(a) [ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)決定3の「eventの欄・flag・既定値・関数名・test名は`docs/design/`に書く」と決定4の「今の姿はdesignが持つ」が、「コードの事実を全部designに写す」と読まれた。
(b) 検査は欠けだけを見て、書き写しや経緯の混入を見ない。
(c) 経緯（task番号・「以前は」・文字数の変遷）が本文に溜まる。
(d) 1文1行や大きさの上限といった形の規則が無い。

## Decision

1. **文書を4層に分ける。**
   - 概念: `docs/design/overview.md`などの全体の文書と、各サブシステムの文書の冒頭。目的・全体の流れの図・責務と境界・不変条件を書き、識別子の一覧と既定値は書かない。人とAIが読む。
   - 地図: 各design文書の本体。「Xを知りたいならコードのここ」という入口、コードから読めない約束と落とし穴、ADRへのリンクを書く。AIが読む。
   - 詳細: 型・event・設定の定義のそばのRustのdoc comment。欄・flag・既定値の意味はここに置く。
   - 経緯: ADRとgitの履歴だけ。
2. **designに書かないもの。** eventの欄・CLIのflag・既定値と閾値の数値・関数名・test名の列挙、コードを読めば分かる手順の書き写し、経緯（task・goal・requestの番号、「以前は」「〜から変えた」、文字数や件数の変遷）。名前は入口として指すのはよいが、一覧にしない。
3. **ADR-t598-1決定3を改める。** ADRに書かないもの（eventのkindと欄名・flagの綴り・JSONの形・既定値と閾値・関数とmoduleとファイルの名前・migrationの番号・testの名前）は、`docs/design/`ではなく、コード（定義のそばのdoc comment）かdesignの地図に書く。地図に書くのはコードから読めない約束とコードへの入口だけである。ADRに書くもの（変えるのに人の判断が要るもの）は変えない。
4. **ADR-t598-1決定4を改める。** 今の姿はコードとdesignが分けて持つ。細かい事実はコードとdoc comment、流れ・境界・不変条件・コードから読めない約束はdesignが持つ。今どうなっているかを指すときはdesignかコードを、なぜそうしたかを指すときはADRを指す。今の決定を1か所で読むための統合ADRを作らないことは変えない。
5. **形: 1文1行。** designの本文は1文ごとに改行する。gitの行単位のマージで同じ段落への別々の追記が衝突せず、grepが段落でなく1文を返すためである。
6. **形: 文書ごとの大きさの予算。** 地図の文書と概念の文書それぞれのbyteの上限と、1行のbyteの上限を持つ。何でも入る巨大な文書を作らず、予算を超えるなら文書を分けるか、細かい事実をdoc commentへ移す。値と決め方、どの文書が概念か、測る範囲は[documents.md](../development/documents.md)の「design」が持つ（値はADRに書かない、ADR-t598-1決定3）。
7. **検査は増やす方向と減らす方向の両方を見る。** 機械の検査は形と大きさ（文書の大きさ・1行の長さ・本文のtask番号）を見て、超えたら落ちる。上限を超える既存の文書は、今の値を上限とする許可の一覧で増えないように止め、書き直して予算内に入れば一覧から外す。runのreviewは欠けに加えて、コードの書き写しと経緯の混入を指摘する（照合とreviewの規則は[ADR-t1942-2](2026-10-07-t1942-2-document-check-and-review-in-both-directions.md)）。

ADR-t598-1の決定1・2・5〜12は変えない。
designから外す細かい事実を消す前に、コードから読めない約束ならdesignの地図に残し、定義の意味ならdoc commentへ移す（goal 159の書き直しの段）。

## Alternatives

- **ADR-t598-1を丸ごと置き換える**: 決定12個のうち変えるのは2個で、ADR-t1091-1により一部を変えるのはamendsにする（人もamendsを指定した）。
- **designに全部書き、検索の道具で必要な部分だけ読ませる**: grepが段落ごと返り、丸ごとの読みも0回で、contextの節約にならない。着地の70%がdesignを変え、衝突の源も残る。
- **行数で大きさを測る**: 日本語の1行は長く、行数が大きさを表さない（ADR-t1453-2決定6と同じ理由）。
- **大きさの検査だけを入れ、内容はreviewに任せない**: 予算内でも書き写しと経緯は溜まる。逆にreviewだけでは大きさの伸びが止まらなかった。
- **細かい事実を別のdesign文書（参照表）に移す**: 書き写しの置き場が移るだけで、コードとの二重持ちとずれが残る。

## Consequences

- designは小さくなり、AIは地図からコードの入口へ進み、細かい事実はdoc commentで読む。workerがsrc/を読む量は増えうるので、goal 159の前後比較で測る。
- doc commentを足す変更がruntimeのcrateに入る。runtimeの挙動は変えない。
- 4層と予算を満たすまで、既存の大きな文書は許可の一覧で止め、goal 159の書き直しの段で文書ごとに予算内へ入れる。
- [documents.md](../development/documents.md)の「ADR」「ADRのID」「design」を同じ変更で合わせる。検査のscript、runのreviewとworkerのpromptの文面は後続のtaskが入れる。
