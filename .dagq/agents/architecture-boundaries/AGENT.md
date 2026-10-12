---
description: このrepositoryのruntimeのレイヤーとコンテキストの境界を検査する
tools: [read, grep, glob]
---

- 分担: L1・L2・L3・L4・L6・L9とL7の時刻、分けた状態のsubmoduleのC3、C8のscriptの部分は`scripts/check-layer-deps.sh`が、C1・C7・X1とC5のportの名前の参照は`scripts/check-context-boundaries.sh`が見る。scriptの規則と許可の一覧の書式、所有の一覧への記録の漏れ（新しいport・table・storeのmodule・transactionの所有と公開）は確認の対象に持たず指摘しない。差分が足したもののうち、scriptが見ない意味の判断だけを見る — [docs/design/architecture.md「検査の範囲」](../../../docs/design/architecture.md#検査の範囲)。
- L5: 状態の判断が時刻を`Clock`か値の引数で受け、判断の中で時計を読まないか — [docs/design/architecture.md「レイヤーの規則」](../../../docs/design/architecture.md#レイヤーの規則)。
- L7の残り・L8: 起動部分が配線だけを持つか（判断とeventのpayloadの組み立て）と、レイヤーの外のmoduleの置き方 — [docs/design/architecture.md「レイヤーの規則」](../../../docs/design/architecture.md#レイヤーの規則)。
- C2・C4・C6: 観測と分析が他のcontextの状態を変えるportのmethodを呼ばないか、新しいuse caseが要るportだけを取るか、他のcontextのeventをkindの定数と型付きの値で読むか — [docs/design/architecture.md「コンテキストの規則」](../../../docs/design/architecture.md#コンテキストの規則)。
- C3の残り: 分けた状態のsubmodule以外の`application::supervise`のsubmoduleが、自分のcontextの`Supervisor`の欄か分けた状態だけを変えるか — [docs/design/architecture.md「コンテキストの規則」](../../../docs/design/architecture.md#コンテキストの規則)。
- C5の残り: contextの決まらないmodule（所有の一覧の`module`の項目に無い`src/application/supervise/mod.rs`とその段、`src/compose/supervisor.rs`・`src/compose.rs`・`src/main.rs`・`src/domain/`ほか）からの他のcontextの内部のportの参照と、一部公開のport（`TaskStore`・`DraftPlannerStore`・`Verifier`ほか、所有の一覧の`partial`のport）のmethodの単位の公開範囲（他のcontextが自分に公開していないmethodを呼んでいないか、traitを名指さず欄や変数を通して使っていないか。公開範囲は所有の一覧の`method`の項目を読む）。あわせて、実行時に組み立てるSQLとtransactionの関数が本体の外のhelperで書くSQL、`use`で入れた別のmoduleのitemをportと取り違えたもの、`item`の貸し先、同じ名前の複数のfnのどれがtransactionか（`scripts/check-context-boundaries.sh`の先頭のコメントの「The review keeps」と同じ範囲） — [docs/design/architecture.md「コンテキストの規則」](../../../docs/design/architecture.md#コンテキストの規則)と[.config/context-boundaries.txt](../../../.config/context-boundaries.txt)。
- C8の残り: 観測と分析が他のcontextのportを名指すのが読むためだけか、`shared`が他のcontextを名指すのが`Queue`・`StateStore`のためだけか、`host`が`execution`を名指すのが`RunLog`の組だけか、`crate::application::X`の再公開を通す名指し — [docs/design/architecture.md「コンテキストの規則」](../../../docs/design/architecture.md#コンテキストの規則)。
- X2・X3: 境界の規則のためにtransactionを分けず述語を外へ出さないか、所有の一覧のtransactionの中で他のcontextの状態を変えるのがそのcontextのinfrastructureの公開する関数を通すか — [docs/design/architecture.md「transactionの規則」](../../../docs/design/architecture.md#transactionの規則)と[.config/context-boundaries.txt](../../../.config/context-boundaries.txt)の`transaction`の項目。
