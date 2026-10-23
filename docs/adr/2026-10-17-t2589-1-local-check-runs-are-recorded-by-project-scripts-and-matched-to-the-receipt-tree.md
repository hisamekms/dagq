---
id: adr-t2589-1
type: adr
title: workerの手元の検証の実行はpjのscriptがrun dirに記録し、runtimeはreceiptのcommitのtreeと一致する記録だけをreviewの根拠に渡し、stress・時間の関門が要るかは宣言したtestのpathと差分からruntimeが決める（ADR-t2268-1決定1をamends）
status: accepted
created: 2026-10-17
updated: 2026-10-17
accepted_on: 2026-10-17
amends:
  - adr-t2268-1 decision 1
amended_by:
  - adr-t3240-1
owners:
  - hisamekms
tags:
  - runtime
  - testing
  - security
related:
  - adr-t2257-1
  - adr-t2268-1
  - adr-t920-1
  - adr-t1707-1
  - adr-t1420-1
  - adr-t1453-1
  - adr-t1895-1
  - adr-t2113-3
  - adr-0029
  - adr-t963-1
---

# ADR-t2589-1: 手元の検証の実行の記録とreceiptのtreeとの照合

## Context

runのreviewのreceipt-evidenceのagentは、workerがreceiptの`tests`のevidenceに手で書いた文（流したコマンド・件数・stressの周回・itのtestの時間の関門のexit status・省いた理由）を読んで手元の検証を審査している。
2026-10-16以降のreviewの113回のうちsend_backは71回で、receipt-evidenceの指摘を含むものが38回あり、その大半は書式（stressや時間の関門を省いた旨と理由が無い、など）で、docsだけの差分にも付いた。
手書きの文は、実行したか・何で終わったか・どの木に対してか、を示せない。worker自身にも、書き忘れをreviewの前に見つける手段が無い。
一方で、どのコマンドを手元の検証とし、どのpathをtestとみなすかはpjと言語ごとに違い、runtimeはそれを固定しない（[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定1、言語の知識はprofileに置く[ADR-t2268-1](2026-10-12-t2268-1-toolchain-knowledge-behind-declared-profiles.md)）。

## Decision

1. **記録の形（a）。**
   手元の検証の1回の実行ごとに、記録を1つ書く。項目はコマンド、exit status、始まりの時刻と所要時間、pjが付ける種類の名前（無くてよい）、その時点の作業treeのtree hash、全文のログの置き場である。
   tree hashは、未commitの変更（gitが無視しないuntrackedを含む）を、本物のindexを変えない一時のindexに全て載せて作ったtree objectのhashとし、そのままcommitしたときのcommitのtreeと同じ値になるものにする。
   tree hashは実行の始まりと終わりの両方で取り、食い違えば実行の間に木が変わったとして、その記録は木を持たないもの（決定3で一致しない記録）にする。
   ログは記録と同じdirに置く。記録とログには大きさの上限を置き、上限は実装が持つ。
2. **書く者と書く先の渡し方（a）。**
   記録を書くのはpjのscriptで、どのコマンドを記録するかもpjのscriptが決める。runtimeは記録するコマンドを知らない。
   runtimeは、run dirの下の記録のdirの場所をworkerのturn（最初のturn・revise・答え・resume）のenvで渡す。値はworkerから見えるpathで、containerで動くturnではcontainerにmountしたrun dirの中のpathにする。
   envが無いとき（人の手元、dagqの外）はscriptは記録を書かず、今と同じに動く。
   `integrate`の検証・landing recheck・e2eなどruntimeが流すコマンドにはこのenvを渡さない。それらの結果は既にruntimeが記録している。
3. **照合の規則（b）。**
   runtimeは、receiptのcommitのtree（containerのrunでは制御側に取り込んだrun branchのcommit）と記録のtree hashを比べ、一致する記録だけを根拠にする。
   一致しない記録は前の木での実行として根拠にせず、件数だけを示す。読めない・形の壊れた・上限を超えた記録は根拠にせず、件数だけを示す。
   記録が無いことも、壊れた記録があることも、receiptの受理を変えず、runを止めない。新しい関門は作らない。
4. **記録の信頼の立場（b）。**
   記録はworkerが書けるrun dirのdataで、workerはそれを偽りうる。記録はreceiptと同じ信頼しないactorの出力（[Security](../design/security.md)の`UntrustedAgent`）で、runtimeは改ざんを検出しようとしない（advisory）。
   照合で弾くのは古い木の記録と壊れた記録だけで、偽りは弾かない。着地の安全は今どおり`integrate`の検証が制御側で流し直すことに依り、記録には依らない。
   runtimeは記録とログをrun dirの他のfileと同じくsymlinkを辿らずに読み、実行も解釈もしない。
5. **reviewへの経路（c）。**
   runtimeは、一致した記録の項目と、次の決定6の判定と、一致しない・壊れた記録の件数を、reviewの資料の節としてreceipt-evidenceのagentに渡す。ログの本文は渡さない。
   記録で示せる事項（実行したか、exit status、時間、どの木か）と、決定6で示せる省略の理由は、workerの手書きのevidenceに求めず、reviewはその欠けを差し戻しの理由にしない。
   receiptの`tests`のevidenceは残し、記録で示せないこと（testの範囲を選んだ理由、流せなかった理由、許可の一覧に足した項目の理由など）だけを書く場所にする。
6. **変えたtestの有無と省略の判定（d）。**
   pjは`dagq.toml`に、testとみなすpathのglobと、手元の関門（このrepositoryではstressとitのtestの時間の関門）ごとに、それを要するtestのpathと関門の記録の種類の名前を宣言する。
   runtimeは宣言パスの検査と同じ差分（mainとreceiptのcommitのmerge-baseからreceiptのcommitまで。[ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)決定3。resumeでrebaseしても他のtaskが着地した変更を数えない）を宣言に当て、関門ごとに要る・要らないを決め、要らない関門は「変えたtestが無い」を理由に省いたとしてreviewに渡す。
   1つのfileがtestとtestでないコードを持つ言語（Rustの`#[cfg(test)]`など）で、差分の行がtestの関数に当たるかを見分けるのは言語の知識なので、toolchainのprofileが絞り込む。
   これはpathに結びつくので、そのfileにpathsが当たる宣言のprofileだけを使う（ADR-t2268-1決定6）。宣言の無いrepositoryで暗黙に当たるRustのprofile（同決定9）もその間は同じく絞る。
   ADR-t2268-1決定1が挙げるprofileの持ち物に、この見分け（差分の行がtestの関数の中か）を足す。
   当たるprofileが無いか、profileが見分けられないfileでは、宣言のglobに当たれば要るとする（省かない側に倒す）。
   関門の宣言が無いpjでは、runtimeは省略を判定せず、関門の節を資料に出さない。
   差分のpathのglobで要否を決める形は、e2eの要否（[ADR-t963-1](2026-09-29-t963-1-e2e-required-by-diff-and-run-in-full-before-auto-update.md)決定2）と同じで、それとは別の宣言にする（e2eはruntimeが流し、この関門はworkerが流す）。
7. **LLMに残す審査（d）。**
   受け入れ条件の各項目と根拠の対応づけ（[ADR-t1420-1](2026-10-03-t1420-1-worker-maps-each-acceptance-criterion-before-the-receipt.md)）と、testの範囲の選び方の妥当さ（変更に対して関係するtestを選んだか、全体を比べるtestを要るときに流したか）は、引き続きreceipt-evidenceのLLMが見る。
   runtimeが決めるのは、記録の照合と、関門が要るかの判定だけである。要る関門の記録が無い・exit statusが失敗のときの扱いもLLMが手書きのevidenceと合わせて判じる。
8. **pjとruntimeの分担と、このrepositoryの値（e）。**
   runtimeが持つのは、記録のdirを渡すこと、tree hashとの照合、宣言と差分からの判定、資料への載せ方だけで、言語・道具・scriptの名前と値を持たない。
   このrepositoryでは、workerの手元のcargoを包むscriptが全ての実行を記録してstressの実行に種類を付け、itのtestの時間の関門のscriptが自分の実行を関門の種類で記録する。testのglobと関門の宣言は`dagq.toml`に置き、ファイルの中のunit testの見分けは組み込みのRustのprofileが持つ。stressの対象（ADR-t920-1決定1）のうちprofileで見分けないpathは省かない側に倒れる。
9. **既存の決定との関係。**
   workerが手元でstressと時間の関門を流すこと（[ADR-t920-1](2026-09-28-t920-1-light-worker-stress-and-heavy-repetition-in-scheduled-ci.md)決定1、[ADR-t1707-1](2026-10-05-t1707-1-time-gate-for-added-or-changed-integration-tests.md)決定3）、関門の対象、落ちたときの扱いは変えない。変わるのは、その根拠の運び方と省略の理由を誰が示すかだけで、これらのADRの決定は変えない。既存のADRで変えるのは、profileの持ち物を足すADR-t2268-1決定1だけである。

## Alternatives

- **reviewの段で手元の検証を流し直す**: reviewのjobは読み取りだけで、testを流すとhostかcontainerの負荷とreviewの時間が大きく増える。着地の検証が既に関門として流し直しており、二重になる。
- **手書きのevidenceのまま書式を緩める**: 差し戻しは減るが、実行したか・どの木かを示せないまま審査を弱めるだけになる。
- **runtimeがworkerのコマンドを包んで記録する（shellのhookなど）**: どのコマンドが検証かをruntimeが知ることになり、pjに依らない原則（ADR-t2257-1決定1）に反する。providerとcontainerごとに包み方も変わる。
- **記録に署名やruntimeだけが書ける置き場を使って改ざんを検出する**: worker（resumeを含む）と同じuser・同じcontainerで動くscriptが書く以上、鍵か置き場をworkerから隠せず、安全も着地の検証に依るので費用に見合わない。
- **treeが一致しない記録も、差分がtestに触れなければ根拠にする**: 照合の規則が複雑になり、どの変更が結果を変えうるかをruntimeが判じることになる。厳密な一致にして、commitの後に流し直す費用を受け入れる。
- **変えたtestの判定をtoolchainのprofileだけに置く**: testのpathの置き方はpjごとに違うので、globは`dagq.toml`、fileの中の見分けだけをprofileに分けた。

## Consequences

- receipt-evidenceの書式の指摘（実行の有無・exit status・省略の理由）は、記録と判定が資料に載る範囲で無くなる。
- workerはcommitの後に検証を流す（またはcommitの前に流した木のままcommitする）必要がある。commitの後に1行でも直せば、記録はその木の根拠にならず流し直しになる。
- 記録は偽りうるので、記録があることは安全の根拠にならない。安全は今どおり`integrate`の検証とmainのCIが持つ。
- 宣言を持たないpjと、scriptが記録を書かないpjでは、今の手書きのevidenceの審査が続く。
- 1回の長い実行の間に木を変えると記録は根拠にならないので、検証は木を止めて流すことになる。
