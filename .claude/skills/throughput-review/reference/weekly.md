# 毎週の見直しの host の前提と当てはめの 1〜4

[SKILL.md](../SKILL.md) の「4. 毎週の見直し」の、この host の前提と当てはめの 1〜4（5 と報告は SKILL.md）。

この host の前提（変わったらここも直す）:

- 8 コア / 16GB。supervisor の並列数は `dagq.toml` の `[supervisor]` の `parallel = 3`
- `dagq.toml` の `[run.env]`: `CARGO_BUILD_JOBS = "4"`（worker 3 本と integrate 1 本で合計 16 並列 = コア数の 2 倍を目安）、`RUST_TEST_THREADS = "6"`、`NEXTEST_TEST_THREADS = "6"`、`RUSTC_WRAPPER = "sccache"`
- 着地は 1 本ずつ直列で、runtime の task の検証（`cargo llvm-cov nextest`）が 1 件あたり数分〜10 分かかる

当てはめ:

1. **算数の確認**: 着地数/時 ≈ `slot_usage` × 3 ÷ 1 件あたりの枠の時間（`phase.work` + `phase.validate` + `phase.wait_to_land` の平均から人の答え待ちを引いたもの）。両辺が大きくずれたら、先に記録か読み方を疑う
2. **制約を 1 つ特定**: `slot_usage` が 1 に張り付き、`load_per_core` の p90 が 1 以上（8 コアで load 8 以上）なら CPU が制約。並列数（`dagq.toml` の `[supervisor]` の `parallel`）を上げず、`cpu_per_landing`（`.rustc`・`.cargo`・`.claude` の内訳）を下げる手を探す。`landing_utilization` が 1 に近いか `.peak` が 1 で `landing_queue_depth` > 0 なら着地の直列が制約（1 件の検証が約 6〜10 分なら 6〜10 件/時で頭打ち）。`ask_wait` が大きければ人待ち
3. **枠の時間のパレート**: `phase.work`・`phase.validate`・`phase.wait_to_land` を中央値 × 件数の大きい順に並べ、一番上だけを割る（`land_phase.*`、`phase.startup` は `work` の内側）
4. **外れ値**: p90・max と、`stats --full` の長い run を `timeline RUN` で読む。`first_pass_rate` と `verification_failed_rate` でやり直しの量を見る
