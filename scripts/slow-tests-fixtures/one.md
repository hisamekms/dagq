## 遅い test（nextest）

終了した test のある log 1 本のうち 1 本を数えた（途中で止まった log を除く）。
採用した log の失敗 1 本（log ごとに数える）は時間の表に含めない。

| 範囲 | 本数 | 合計（秒） | 全体の合計に占める割合 |
| --- | ---: | ---: | ---: |
| 全体 | 9 | 80.3 | 100% |
| 1 秒を超える | 5 | 78.5 | 98% |
| 5 秒を超える | 2 | 71.0 | 88% |
| 30 秒を超える | 1 | 65.0 | 81% |

### 上位 9 本

| # | 秒 | test |
| ---: | ---: | --- |
| 1 | 65.000 | `dagq::it runtime_x::a_very_slow_test` |
| 2 | 6.000 | `dagq::it runtime_x::a_slow_test` |
| 3 | 4.000 | `dagq::it runtime_y::a_flaky_test` |
| 4 | 2.000 | `dagq::it runtime_y::a_coloured_test` |
| 5 | 1.500 | `dagq domain::a_slower_unit_test` |
| 6 | 0.750 | `dagq domain::another_unit_test` |
| 7 | 0.500 | `dagq-broker broker::a_crate_test` |
| 8 | 0.300 | `dagq::it runtime_y::a_quick_test` |
| 9 | 0.250 | `dagq domain::a_fast_unit_test` |

### test binary ごと

| test binary | 本数 | 合計（秒） | 全体の合計に占める割合 |
| --- | ---: | ---: | ---: |
| `dagq::it` | 5 | 77.3 | 96% |
| `dagq` | 3 | 2.5 | 3% |
| `dagq-broker` | 1 | 0.5 | 1% |
