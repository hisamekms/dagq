---
description: このrepositoryのtestの構成と書き方を検査する
---

- A-092・A-117: testの書き方の適合性 — [docs/development/testing.md「testの書き方」](../../docs/development/testing.md#testの書き方)。
- A-115・A-116・A-118: testの置き場所の適合性 — [docs/development/testing.md「testの置き場所」](../../docs/development/testing.md#testの置き場所)。
- A-262: 判断と境界のtestの適合性 — [docs/development/testing.md「判断と境界のtest」](../../docs/development/testing.md#判断と境界のtest)。
- itのtestの時間の関門の適合性（足した・変えた`tests/it`のtestが判断のcaseをloopしてcaseごとにfixtureやsupervisorを起動し直していないか、`.config/it-slow-allow.toml`に足した項目の理由が節の挙げる理由（守る境界か移し替えの行き先）に当たるか） — [docs/development/testing.md「判断と境界のtest」](../../docs/development/testing.md#判断と境界のtest)。
- A-263: shellに渡るpathの`shell_path`による引用の適合性 — [docs/development/testing.md「testの書き方」](../../docs/development/testing.md#testの書き方)。
- A-120: testファイルの行数の適合性 — [docs/development/testing.md「testファイルの行数」](../../docs/development/testing.md#testファイルの行数)。
- A-122: e2eの適合性 — [docs/development/testing.md「e2e」](../../docs/development/testing.md#e2e)。
- A-131・A-133: e2eの印の適合性 — [docs/development/testing.md「e2eの印」](../../docs/development/testing.md#e2eの印)。
- A-137: 待ちの上限の適合性 — [docs/development/testing.md「待ちの上限」](../../docs/development/testing.md#待ちの上限)。
