---
description: このrepositoryのruntimeのレイヤーとコンテキストの境界を検査する
---

- 分担: L1・L2・L3・L4・L6と許可の一覧の書式は`scripts/check-layer-deps.sh`に任せて見ず、scriptが見ない意味の境界だけを見る — [docs/design/architecture.md「検査の範囲」](../../../docs/design/architecture.md#検査の範囲)。
- L7・L8: 起動部分とレイヤーの外のmoduleの置き方の適合性 — [docs/design/architecture.md「レイヤーの規則」](../../../docs/design/architecture.md#レイヤーの規則)。
- C1・C2・C5: contextをまたぐ参照が公開したportと値だけかの適合性 — [docs/design/architecture.md「コンテキストの規則」](../../../docs/design/architecture.md#コンテキストの規則)と、各contextの節（[計画管理](../../../docs/design/architecture.md#計画管理)・[実行と着地](../../../docs/design/architecture.md#実行と着地)・[観測と分析](../../../docs/design/architecture.md#観測と分析)・[host運用](../../../docs/design/architecture.md#host運用)）の「公開するport」。
- C3: `Supervisor`の欄を変えるsubmoduleの適合性 — [docs/design/architecture.md「コンテキストの規則」](../../../docs/design/architecture.md#コンテキストの規則)。
- X1・X2・X3: 境界をまたぐtransactionの適合性 — [docs/design/architecture.md「transactionの規則」](../../../docs/design/architecture.md#transactionの規則)と[「境界をまたぐtransaction」](../../../docs/design/architecture.md#境界をまたぐtransaction)。
- C4・C7: 新しいuse caseとportの取り方の適合性 — [docs/design/architecture.md「コンテキストの規則」](../../../docs/design/architecture.md#コンテキストの規則)。
- L5・C6: 状態の判断が時刻と観測（eventのpayload）を値で受けるかの適合性 — [docs/design/architecture.md「レイヤーの規則」](../../../docs/design/architecture.md#レイヤーの規則)と[「コンテキストの規則」](../../../docs/design/architecture.md#コンテキストの規則)。
- 境界の記録: 違反を足す・直す差分の[「今の違反と行き先」](../../../docs/design/architecture.md#今の違反と行き先)の行と許可の一覧の項目、新しいportとtransactionの所有と公開の記載の適合性（designの文書の一般の対応はdesign-consistencyが見る） — [docs/design/architecture.md「検査の範囲」](../../../docs/design/architecture.md#検査の範囲)。
