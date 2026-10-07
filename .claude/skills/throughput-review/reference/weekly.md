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

## docs の4指標

毎週の見直しで、docs/design の4指標（総量と伸び・docs/design を変えた着地の割合・docs の衝突と claim の控え・道具の結果に占める docs）を `scripts/docs-metrics.py` で数え、基準値と前週と並べる。定義と基準値は [docs/plans/docs-slim.md](../../../../docs/plans/docs-slim.md)。repository の root（main が着地の branch の checkout）で、inbox か人が流す（M4 は数分かかる）。期間は先週の月曜 00:00Z から今週の月曜 00:00Z（`[since, until)`）。

```sh
since=2026-10-12 until=2026-10-19   # 先週の月曜と今週の月曜（UTC の日付）に置き換える
out=~/.local/share/dagq-hostmetrics/docs-slim; mkdir -p "$out"
~/.local/bin/dagq stats --since "${since}T00:00:00Z" --until "${until}T00:00:00Z" > "$out/stats-${since}_${until}.json"
python3 scripts/docs-metrics.py --since "$since" --until "$until" --stats "$out/stats-${since}_${until}.json" \
  --claude-projects ~/.claude/projects --format json > "$out/${since}_${until}.json"
python3 scripts/docs-metrics.py --input "$out/${since}_${until}.json"   # 今週の表
ls "$out"; python3 scripts/docs-metrics.py --input "$out/<前週の since>_<前週の until>.json"   # 前週の表
```

- M3 は `dagq stats --since` の JSON（上で保存したファイル）を、M4 は `~/.claude/projects` を渡す。script は queue を読まない
- 報告は指標ごとに今週・前週・docs-slim.md の基準値を並べる: M1 は最後の日の総 byte と1日あたりの伸び・30 KiB 超の数、M2 は2つの割合と追加・削除の行、M3 は docs の衝突の割合と控えの件数・expired・待ちの時間、M4 は docs と src の割合・grep/rg と範囲の Read の回数・中央値と p90、(a) 上位文書の grep の結果の中央値と p90、(b) worker の run ごとの src/ の読みの中央値と p90
- **着地の後の最初の週次の見直しでは、基準の期間も流して保存する**: `since=2026-09-29 until=2026-10-06` にして上の 2〜5 行目を流し、`$out/2026-09-29_2026-10-06.json` を残す（M4 の同じ定義の前の値。task 1957 の前後比較が使う）。Claude Code は既定で30日より古い会話記録を消すので、2026-10-28 より前に流す
