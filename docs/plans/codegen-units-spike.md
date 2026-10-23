---
id: plan-codegen-units-spike
type: plan
title: スパイク：codegen-units = 64（V5）は host で既定（V0）より test の build の合計の peak を下げ、時間を延ばさないか
status: completed
created: 2026-10-24
owners:
  - hisamekms
tags:
  - planning
  - performance
  - measurement
related:
  - plan-build-memory
  - development-operations
---

# スパイク：codegen-units = 64 の効きの測り直し

問いは「`CARGO_PROFILE_DEV_CODEGEN_UNITS=64`（V5）は、この repository の host で既定（V0）より (ii) `cargo test --locked --no-run` の合計の peak を下げ、かつ (i) `cargo build --locked --lib` と (ii) の時間を延ばさないか」。
[build-memory](build-memory.md) の測定では V5 の比が (i) 時間 0.54、(ii) 合計の peak 0.88・時間 0.71 だったが、V0 の 2 周の揺れと重なって分けられなかった。
ここでは V0 と V5 だけを 4 周、交互の組で測り、測る前に決めた規則で判定した。

判定は **does_not_hold**（下の「判定」）。

## 環境

- 対象の commit: `6cbd1efd51365db383b289087e2cfafe225247eb`（この run の base）。
- host: Apple M1（MacBookAir10,1、8 コア）、メモリ 16 GiB（`hw.memsize` 17179869184 byte）、macOS 14.7.4（23H420）。
- 道具: `rustc 1.98.1 (48a229cea 2026-09-01)`、`cargo 1.98.1 (797e8a9bc 2026-08-05)`。
- provider と経路: 本番 queue の Spike の run（task 3196）の Claude Code の headless の worker の turn の中で、自分の worktree の shell から流した。
- 時刻: 2026-10-24 01:55:26〜02:31:05（JST）。

## 条件

- 測る場所と前提は [build-memory](build-memory.md) の「条件」と同じ:
  - host の自分の worktree で、1 本ずつ直列に流した。
  - `RUSTC_WRAPPER` を空にして外した（rustc が sccache の server の子になると cargo の process の木に入らないため）。
    依存の crate も毎回 compile した。
  - `[run.env]` の並列度（`CARGO_BUILD_JOBS=4`・`RUST_TEST_THREADS=6`・`NEXTEST_TEST_THREADS=6`）を env で与えた。
  - `Cargo.toml` と `dagq.toml` は変えず、V5 は env の `CARGO_PROFILE_DEV_CODEGEN_UNITS=64` で上書きした。
    `Cargo.toml` の `[profile.dev]` は `debug = "line-tables-only"`、`[profile.dev.package."*"]` は `debug = false`（codegen-units と incremental は cargo の既定）。
  - variant ごとに worktree の中の別の `CARGO_TARGET_DIR`（`target/build-memory/<variant>`）を使い、commit していない。
  - 値の採り方（1 秒ごとの `ps` の木の RSS と `/usr/bin/time -l`）は `build-memory/measure.sh` の冒頭のとおり。
- cold を作り直す単位は「周 × variant」: 各周の各 variant の (i) の前にその variant の `CARGO_TARGET_DIR` を消し、(i)→(ii) を続けて流し、(ii) の後に消した。
- 流した順（4 周、交互）: 1 周目 V0→V5、2 周目 V5→V0、3 周目 V0→V5、4 周目 V5→V0。
  各 variant の平均の時刻をそろえるためで、周の境界で同じ variant が続くのは 1→2・3→4 の V5 と 2→3 の V0（どれも target を消して cold から流した）。
- 各 run の開始と終了の load（1 分）と、同時に動いていた本番の worker の turn と verification の数は、下の「周ごと」の表の右の 3 列。
  - load（1 分）は 4.30〜13.31 の間で動いた（build-memory の測定の 4.05〜28.17 より狭い）。
  - 同時の worker の turn は各 run の間で 0〜1 本、verification は 0〜1 本だった。
  - 数え方は build-memory と同じで、測定の後に本番 queue の各 run の dir のファイルの時刻から区間を作り直した（`build-memory/count-concurrent.py` の冒頭。自分の run の dir は除いた）。
    worker の turn は `turns/turn-N.err` の作成から `turn-N.jsonl` の最後の書き込みまで、verification は `integrate-N-verify-M.log` の最後の書き込みを終わりとし、同じ integrate の 1 つ前のコマンドのログの最後の書き込みを始まりにした。
    表の「最大」は run の間を 60 秒ごとに数えた最大。
- 使った時間: 測定は 35 分 39 秒（上限 3 時間）。
  4 周を 1 回だけ流し、16 本の run はどれも exit 0 で、流し直しは無い。

## 打ったもの

```sh
# 4 周を流す（cgu-runs.csv・cgu-samples.csv に書く。measure.sh の one を BUILD_MEMORY_PREFIX=cgu- で呼ぶ）
sh docs/plans/build-memory/measure-cgu.sh all
# 同時に動いていた本番の worker の turn と verification を数える
python3 -I docs/plans/build-memory/count-concurrent.py ~/.local/share/dagq/<queue>/runs <自分の run id> \
  > docs/plans/build-memory/cgu-concurrent.csv
# 表と判定を作る
python3 -I docs/plans/build-memory/summarize-cgu.py
```

## 表

下の 3 つは `python3 -I docs/plans/build-memory/summarize-cgu.py` が `build-memory/cgu-runs.csv` と `build-memory/cgu-concurrent.csv` から作ったものを貼った（「判定に当てはめた値」も同じ出力で、下の「判定」が使う）。
CSV の行の順は流した順。
1 process の peak の process はどの run も `rustc(dagq)` だった。

### 周ごと（流した順）

| # | 周 | variant | 対象 | 開始（UTC） | 終了（UTC） | 秒 | exit | 1 process の peak（KiB） | その process | 合計の peak（KiB） | time -l の max RSS（byte） | load 1 分（前→後） | worker の turn（前→後、最大） | verification（前→後、最大） |
| ---: | ---: | --- | --- | --- | --- | ---: | ---: | ---: | --- | ---: | ---: | --- | --- | --- |
| 1 | 1 | V0 | build | 2026-10-23T16:55:26Z | 2026-10-23T16:56:58Z | 92 | 0 | 4071616 | rustc(dagq) | 4108064 | 4288364544 | 4.30→5.38 | 0→0、0 | 0→0、0 |
| 2 | 1 | V0 | test-build | 2026-10-23T16:56:58Z | 2026-10-23T16:59:19Z | 141 | 0 | 5580704 | rustc(dagq) | 7366720 | 5830508544 | 5.38→7.09 | 0→1、1 | 0→0、0 |
| 3 | 1 | V5 | build | 2026-10-23T16:59:19Z | 2026-10-23T17:01:31Z | 132 | 0 | 4159184 | rustc(dagq) | 4208400 | 4353490944 | 7.01→13.31 | 1→0、1 | 0→0、0 |
| 4 | 1 | V5 | test-build | 2026-10-23T17:01:31Z | 2026-10-23T17:03:58Z | 147 | 0 | 5658688 | rustc(dagq) | 8056240 | 5918998528 | 13.31→6.85 | 0→0、0 | 0→0、0 |
| 5 | 2 | V5 | build | 2026-10-23T17:03:59Z | 2026-10-23T17:05:46Z | 107 | 0 | 2623664 | rustc(dagq) | 2647248 | 2741501952 | 6.85→8.80 | 0→0、0 | 0→1、1 |
| 6 | 2 | V5 | test-build | 2026-10-23T17:05:46Z | 2026-10-23T17:09:05Z | 199 | 0 | 4677360 | rustc(dagq) | 8480432 | 4800512000 | 8.80→9.60 | 0→0、0 | 1→0、1 |
| 7 | 2 | V0 | build | 2026-10-23T17:09:06Z | 2026-10-23T17:10:38Z | 92 | 0 | 3515248 | rustc(dagq) | 3539072 | 3649454080 | 9.23→6.80 | 0→0、0 | 0→0、0 |
| 8 | 2 | V0 | test-build | 2026-10-23T17:10:38Z | 2026-10-23T17:13:24Z | 166 | 0 | 3777440 | rustc(dagq) | 5723664 | 4178935808 | 6.80→10.01 | 0→0、0 | 0→0、0 |
| 9 | 3 | V0 | build | 2026-10-23T17:13:24Z | 2026-10-23T17:14:47Z | 83 | 0 | 4234960 | rustc(dagq) | 4283488 | 4390010880 | 10.01→7.98 | 0→0、0 | 0→0、0 |
| 10 | 3 | V0 | test-build | 2026-10-23T17:14:48Z | 2026-10-23T17:17:52Z | 184 | 0 | 5072528 | rustc(dagq) | 7534032 | 5204590592 | 7.98→8.53 | 0→0、0 | 0→0、0 |
| 11 | 3 | V5 | build | 2026-10-23T17:17:52Z | 2026-10-23T17:19:26Z | 94 | 0 | 4094720 | rustc(dagq) | 4143456 | 4389011456 | 8.53→6.40 | 0→0、0 | 0→0、0 |
| 12 | 3 | V5 | test-build | 2026-10-23T17:19:26Z | 2026-10-23T17:21:51Z | 145 | 0 | 3594144 | rustc(dagq) | 5616640 | 3722854400 | 6.40→6.83 | 0→0、0 | 0→1、1 |
| 13 | 4 | V5 | build | 2026-10-23T17:21:52Z | 2026-10-23T17:23:44Z | 112 | 0 | 4190512 | rustc(dagq) | 4239152 | 4332552192 | 6.83→11.39 | 0→0、0 | 1→1、1 |
| 14 | 4 | V5 | test-build | 2026-10-23T17:23:44Z | 2026-10-23T17:26:46Z | 182 | 0 | 4761888 | rustc(dagq) | 8319360 | 4908662784 | 11.39→12.10 | 0→1、1 | 1→0、1 |
| 15 | 4 | V0 | build | 2026-10-23T17:26:47Z | 2026-10-23T17:28:28Z | 101 | 0 | 3757360 | rustc(dagq) | 3781264 | 3910778880 | 12.10→7.21 | 1→0、1 | 0→0、0 |
| 16 | 4 | V0 | test-build | 2026-10-23T17:28:28Z | 2026-10-23T17:31:05Z | 157 | 0 | 4265184 | rustc(dagq) | 7232352 | 4425318400 | 7.21→7.44 | 0→1、1 | 0→0、0 |

### 周ごとの比 V5/V0 と 4 周の中央値

比は同じ周の V5 の値 ÷ V0 の値（単位なし）。中央値は 4 周の比の 2 番目と 3 番目の平均。

| 対象 | 値 | 1 周目 | 2 周目 | 3 周目 | 4 周目 | 1 未満の周 | 中央値 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| (i) | 1 process の peak | 1.02 | 0.75 | 0.97 | 1.12 | 2 | 0.99 |
| (i) | 合計の peak | 1.02 | 0.75 | 0.97 | 1.12 | 2 | 1.00 |
| (i) | 時間 | 1.43 | 1.16 | 1.13 | 1.11 | 0 | 1.15 |
| (ii) | 1 process の peak | 1.01 | 1.24 | 0.71 | 1.12 | 1 | 1.07 |
| (ii) | 合計の peak | 1.09 | 1.48 | 0.75 | 1.15 | 1 | 1.12 |
| (ii) | 時間 | 1.04 | 1.20 | 0.79 | 1.16 | 1 | 1.10 |

### 判定に当てはめた値

- (ii) の合計の peak の比: 1 未満の周 1 / 4、中央値 1.122
- 時間の比の中央値: (i) 1.148、(ii) 1.101
- 判定: does_not_hold

## 判定

規則（測る前に決めたもの。結果を見て動かしていない）: 周ごとに同じ周の V0 と V5 を組にして比 V5/V0 を取り、4 周の比の中央値は 2 番目と 3 番目の平均とする。

- holds: (ii) の合計の peak の比が 4 周のうち 3 周以上で 1 未満、かつその中央値が 0.95 以下、かつ (i) と (ii) の時間の比の中央値がどちらも 1.0 以下。
- does_not_hold: 4 周がそろい、(ii) の合計の peak の比の中央値が 1.0 以上か 1 未満の周が 1 周以下、または (i) か (ii) の時間の比の中央値が 1.05 を超える。
- unresolved: それ以外（4 周がそろわない、または値が holds と does_not_hold の間）。

判定: **does_not_hold**。

根拠: 4 周はそろい（16 本とも exit 0）、does_not_hold の条件のすべてに当たった。

- (ii) の合計の peak の比の中央値は 1.12（1.0 以上）。
- (ii) の合計の peak の比が 1 未満の周は 3 周目（0.75）の 1 周だけ（1 周以下）。
- 時間の比の中央値は (i) 1.15・(ii) 1.10 で、どちらも 1.05 を超える。
  (i) は 4 周とも V5 が遅かった（1.11〜1.43）。
- holds の条件は (ii) の合計の peak（1 未満が 3 周以上、中央値 0.95 以下）も時間（中央値 1.0 以下）も満たさない。

読み:

- build-memory の V5 の速さ（(i) 0.54・(ii) 0.71）は、load の揺れだったと読める。
  (i) の 0.54 は主に V0 の 1 周目の 332 秒（load 12.19→20.60。同じ周の V5 は 9.50→7.26 で 98 秒）から来ている。
  逆に 2 周目は V5 が load 15.34→11.51 で 137 秒、V0 が 4.39→7.48 で 107 秒と V5 が遅く、2 周の平均の比は 1 周目の差に引っ張られていた。
  この測定は load が低くそろっていて（4.30〜13.31）、V0 の (i) は 83〜101 秒と build-memory の V0（107〜332 秒）より短く揺れも小さい。
- cargo の dev profile の codegen-units の既定は incremental の build で 256 なので、V5 は workspace の crate の codegen-units を 256 から 64 に減らす変更に当たる（依存の crate は incremental でなく既定 16 で、V5 では 64 に増える）。
  この host（M1、8 コア、`CARGO_BUILD_JOBS=4`）でそれが速さとメモリのどちらにも効かなかった仕組みは、判定に要らないので測っていない。
- 1 process の peak の比も (i) 中央値 0.99、(ii) 1.07 で、下げる向きは見えない。

## 後続の計画への推奨

- 判定が does_not_hold なので、`Cargo.toml` を変えず、この件を閉じる（codegen-units = 64 を `[profile.dev]` に置く implementation の task は足さない）。
- build-memory の「採るものと置き場の推奨」の V5 の項（container のスモークで確かめてから採る）は、この測定では書き換えていない。
  host では時間と合計の peak のどちらも効かないと出たので、再計画では V5 を container のスモーク（task 2183）で測る対象からも外してよいと見立てる。
- operations.md の host.toml の推奨（(ii) の合計の peak から決めている）は、V5 を採らないので変える必要は無い。

## 測定の出力の区分と作り直せる範囲

- `build-memory/measure-cgu.sh`・`build-memory/count-concurrent.py`・`build-memory/summarize-cgu.py`・`build-memory/cgu-runs.csv`・`build-memory/cgu-samples.csv`・`build-memory/cgu-concurrent.csv` は、この 1 回きりの測定の材料で、SSOT でも他の仕組みが読む共有の記録でもない。
  build-memory の `runs.csv`・`samples.csv` は書き換えていない（`measure.sh` には CSV の名前の頭を分ける `BUILD_MEMORY_PREFIX` だけを足した）。
- この文書の「表」と「判定」の値は `python3 -I docs/plans/build-memory/summarize-cgu.py` で `cgu-runs.csv` と `cgu-concurrent.csv` から作り直せる。
- `cgu-concurrent.csv` は本番 queue の run の dir が残っている間だけ `count-concurrent.py` で作り直せる。
- `cgu-runs.csv` と `cgu-samples.csv` は同じ commit で `sh docs/plans/build-memory/measure-cgu.sh all` を流し直すと作れるが、host の load で値は同じにならない。
  cargo の出力と `time -l` の出力は `$TMPDIR/cgu-build-memory-logs` に置き、commit していない。
