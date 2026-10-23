---
id: plan-build-memory
type: plan
title: dagq の build・test の process ごとと合計の peak のメモリの測定と、下げる手（CARGO_BUILD_JOBS・codegen-units・CARGO_INCREMENTAL・debuginfo）の効き、host.toml の machine と container の上限の推奨
status: completed
created: 2026-10-23
owners:
  - hisamekms
tags:
  - planning
  - performance
related:
  - plan-container-runtime-spike
  - plan-container-oom-detection
  - development-operations
  - adr-t2257-1
---

# dagq の build・test の peak のメモリと、host.toml の推奨

問いは「この repository の 1 本の build と test がどれだけのメモリを要り、それを下げる手がどれだけ効くか」。
答えが、worker と verification を container に切り替える前に host.toml に入れる machine と container の上限の推奨（[運用](../development/operations.md)の「containerのmachineの資源（`host.toml`）」）を決める。
きっかけは container の Spike の (d) の 2 回目（goal 38 の note 297078）で、machine 4 CPU / 8 GiB の中の dagq の lib の rustc 1 本が anon-rss 3856512 kB を使い、2 本の同時の compile が machine 全体の OOM killer に殺されたこと。

- 測ったのは host（macOS）の worktree で、container の中ではない。
  container の中の絶対値は container の runtime のスモーク（task 2183）で確かめる（下の「container との比」）。
- 測ったのは 1 本ずつ直列で、[run.env] の並列度を超えて上げていない。

## 条件

- host: Apple M1（8 コア）、メモリ 16 GiB（`hw.memsize` 17179869184 byte）、macOS 14.7.4（23H420）。
- 道具: `rustc 1.98.1 (48a229cea 2026-09-01)`（host aarch64-apple-darwin、LLVM 22.1.8）、`cargo 1.98.1 (797e8a9bc 2026-08-05)`、cargo-nextest 0.9.146。
- 測った commit: `62a3599364d64b134477ab73bffe6d020157ee03`。
- `Cargo.toml` の `[profile.dev]`: `debug = "line-tables-only"`、`[profile.dev.package."*"]` は `debug = false`（codegen-units と incremental は cargo の既定）。
- `dagq.toml` の `[run.env]` から与えた値: `CARGO_BUILD_JOBS=4`・`RUST_TEST_THREADS=6`・`NEXTEST_TEST_THREADS=6`。
- `RUSTC_WRAPPER` は空にして外した。
  sccache の下では rustc が sccache の server の子になり、cargo の process の木に入らないため。
  sccache が効くのは依存の crate だけで（note 297078）、peak を作る workspace の crate（dagq の lib と test）の compile には関わらない。
  依存の crate も毎回 compile したので、時間は sccache の効いた worker の build より長い。
- 時刻: 2026-10-23 11:30:08〜13:26:34（JST）。
- 同時に動いていた本番の worker と verification（自分の run を除く）:
  - 開始時は worker の turn 1 本と着地の verification 1 本で、終了時は worker の turn 2 本と verification 0 本。
  - 測っている間の worker の turn は 1 分ごとに数えて 0〜2 本（平均 1.2 本）。
    1 分を超える着地の verification は同時に 1 本までで、2 回（11:26〜11:49 と 11:53〜12:13）あった。
  - 数え方: 測定の後に、本番 queue の各 run の dir のファイルの時刻から区間を作り直した。
    worker の turn は `turns/turn-N.err` の作成から `turn-N.jsonl` の最後の書き込みまで。
    verification は `integrate-N-verify-M.log` の最後の書き込みを終わりとし、同じ integrate の 1 つ前のコマンドのログの最後の書き込みを始まりにした。
    開始時と終了時の `ps` では、process の引数に自分以外の run の dir が 2 本ずつ現れ、上の区間と合った。
  load average（1 分）は各 run の前後を表に残し、4.05〜28.17 の間で動いた。
- 対象:
  - (i) `cargo build --locked --lib`（note の 3.9 GB の場面）。
  - (ii) `cargo test --locked --no-run`（test の binary の compile と link）。
    (i) の直後に同じ target で流し、(i) で作ったものの上に足される分を測る。
    IT の binary（`tests/it` ほか）の compile と link はここに含まれる。
  - (iii) `cargo nextest run --locked --lib --bins`（着地の verification の `sh scripts/landing-it.sh` の unit test の部分の形）。
    (ii) の後の V0 の target で流し、compile を含まない実行だけを測る。
    流れた test は 4 回とも 2 つの binary の 3075 件で、すべて passed（nextest の Summary の行）。
    IT の実行は着地ごとに選ぶ IT が変わるので測っていない。
- variant（(i)・(ii)）: V0 既定（上の `[profile.dev]` と `[run.env]`）、V1 `CARGO_BUILD_JOBS=1`、V2 `CARGO_BUILD_JOBS=2`、V3 `CARGO_INCREMENTAL=0`、V4 `CARGO_PROFILE_DEV_CODEGEN_UNITS=16`、V5 `CARGO_PROFILE_DEV_CODEGEN_UNITS=64`、V6 `CARGO_PROFILE_DEV_DEBUG=0`。
  `Cargo.toml` と `dagq.toml` は変えず env で上書きし、variant ごとに worktree の中の別の `CARGO_TARGET_DIR`（`target/build-memory/<variant>`）を使った。
- variant（(iii)）: T6 は `[run.env]` の `NEXTEST_TEST_THREADS=6` のまま、T2 は `NEXTEST_TEST_THREADS=2`。
- cold を作り直す単位は「周 × variant」: 各周の各 variant の (i) の前にその variant の `CARGO_TARGET_DIR` を消し、(i)→(ii) を続けて流した。
- 流した順（交互）:
  - (i)・(ii) は 2 周で、1 周目 V0→V1→…→V6、2 周目 V6→V5→…→V0。
    逆順にして、各 variant の 2 周の平均の時刻をそろえ、host の load の時間の偏りを variant に寄せないようにした。
    同じ variant が続くのは周の境界の V6（1 周目の最後と 2 周目の最初）だけで、この V6 も target を消して cold から流した。
  - (iii) は 2 周で、1 周目 T6→T2、2 周目 T2→T6。
    同じ variant が続くのは周の境界の T2 だけ。
    test の binary は (ii) の V0 の 2 周目の target をそのまま使い、作り直していない。
- 値の採り方（`build-memory/measure.sh` の冒頭）:
  - 1 process の peak: 1 秒ごとに `ps` で `/usr/bin/time` の下の process の木の各 process の RSS（KiB）を採った最大と、その process の名前（rustc は `--crate-name` を添えた）。
    あわせて `/usr/bin/time -l` の maximum resident set size（byte）を並べた。
  - 合計の peak: 同じ採取の各時点の木の RSS の合計の最大。
    container の cgroup が数えるのは合計のため。
  - 1 秒ごとの採取は 1 秒より短い山を取りこぼしうる。
    `/usr/bin/time -l` の値と並べるのはそのためで、どの run でも `time -l` の値（byte）は 1 process の peak（KiB × 1024）の 1.00〜1.05 倍だった。
  - macOS の RSS は共有の page（共有 library など）を process ごとに数えるので、合計は同じ page を重ねて数える。
    また圧縮や swap に出た page は RSS に入らない（測っている間、host の swap は約 1.1 GiB を使っていた）。
- 集計: variant・対象ごとに、peak は 2 周の大きい方（上限を決めるので悪い方を採る）、時間は 2 周の平均。

## 表

表は `python3 -I docs/plans/build-memory/summarize.py` が `build-memory/runs.csv` から作ったものを貼った。
(i)・(ii) の 1 process の peak の process はどの run も `rustc(dagq)` だった（(ii) では lib の test と bin がどちらも crate 名 dagq で、ここでは分けていない）。
(iii) は dagq の lib の unit test の binary（`dagq-<hash>`）だった。

### 周ごと（流した順）

| # | 周 | variant | 対象 | 開始（UTC） | 終了（UTC） | 秒 | exit | 1 process の peak（KiB） | その process | 合計の peak（KiB） | time -l の max RSS（byte） | load 1 分（前→後） |
| ---: | ---: | --- | --- | --- | --- | ---: | ---: | ---: | --- | ---: | ---: | --- |
| 1 | 1 | V0 | build | 2026-10-23T02:30:08Z | 2026-10-23T02:35:40Z | 332 | 0 | 4159728 | rustc(dagq) | 4209424 | 4293607424 | 12.19→20.60 |
| 2 | 1 | V0 | test-build | 2026-10-23T02:35:40Z | 2026-10-23T02:41:14Z | 334 | 0 | 5537088 | rustc(dagq) | 7381504 | 5755600896 | 20.60→15.75 |
| 3 | 1 | V1 | build | 2026-10-23T02:41:14Z | 2026-10-23T02:45:45Z | 271 | 0 | 3632976 | rustc(dagq) | 3681936 | 3764027392 | 15.75→10.82 |
| 4 | 1 | V1 | test-build | 2026-10-23T02:45:45Z | 2026-10-23T02:52:44Z | 419 | 0 | 5518512 | rustc(dagq) | 5567632 | 5801623552 | 10.82→4.60 |
| 5 | 1 | V2 | build | 2026-10-23T02:52:46Z | 2026-10-23T02:54:39Z | 113 | 0 | 3746640 | rustc(dagq) | 3795536 | 4028940288 | 4.60→5.59 |
| 6 | 1 | V2 | test-build | 2026-10-23T02:54:39Z | 2026-10-23T03:00:32Z | 353 | 0 | 5098480 | rustc(dagq) | 7681984 | 5246582784 | 5.59→13.22 |
| 7 | 1 | V3 | build | 2026-10-23T03:00:35Z | 2026-10-23T03:03:35Z | 180 | 0 | 4006768 | rustc(dagq) | 4056592 | 4164419584 | 12.72→17.58 |
| 8 | 1 | V3 | test-build | 2026-10-23T03:03:35Z | 2026-10-23T03:08:28Z | 293 | 0 | 5409248 | rustc(dagq) | 7010128 | 5568086016 | 17.58→20.67 |
| 9 | 1 | V4 | build | 2026-10-23T03:08:30Z | 2026-10-23T03:11:33Z | 182 | 0 | 4121056 | rustc(dagq) | 4169552 | 4295852032 | 19.25→15.14 |
| 10 | 1 | V4 | test-build | 2026-10-23T03:11:33Z | 2026-10-23T03:15:05Z | 212 | 0 | 5221040 | rustc(dagq) | 7122448 | 5418418176 | 15.14→9.50 |
| 11 | 1 | V5 | build | 2026-10-23T03:15:06Z | 2026-10-23T03:16:44Z | 98 | 0 | 3439136 | rustc(dagq) | 3475184 | 3623813120 | 9.50→7.26 |
| 12 | 1 | V5 | test-build | 2026-10-23T03:16:44Z | 2026-10-23T03:19:46Z | 182 | 0 | 3793536 | rustc(dagq) | 6489296 | 3904716800 | 7.26→16.90 |
| 13 | 1 | V6 | build | 2026-10-23T03:19:46Z | 2026-10-23T03:21:23Z | 97 | 0 | 3496592 | rustc(dagq) | 3544928 | 3685859328 | 16.90→10.73 |
| 14 | 1 | V6 | test-build | 2026-10-23T03:21:23Z | 2026-10-23T03:23:50Z | 147 | 0 | 4894832 | rustc(dagq) | 6900960 | 5104975872 | 10.73→9.10 |
| 15 | 2 | V6 | build | 2026-10-23T03:23:51Z | 2026-10-23T03:25:24Z | 93 | 0 | 2800336 | rustc(dagq) | 2823712 | 2929557504 | 9.10→6.93 |
| 16 | 2 | V6 | test-build | 2026-10-23T03:25:24Z | 2026-10-23T03:29:45Z | 261 | 0 | 2858272 | rustc(dagq) | 3572400 | 2975498240 | 6.93→15.34 |
| 17 | 2 | V5 | build | 2026-10-23T03:29:47Z | 2026-10-23T03:32:04Z | 137 | 0 | 2440544 | rustc(dagq) | 2475696 | 2538061824 | 15.34→11.51 |
| 18 | 2 | V5 | test-build | 2026-10-23T03:32:04Z | 2026-10-23T03:35:44Z | 219 | 0 | 3586240 | rustc(dagq) | 5476752 | 3723132928 | 11.51→23.65 |
| 19 | 2 | V4 | build | 2026-10-23T03:35:49Z | 2026-10-23T03:39:40Z | 231 | 0 | 3504288 | rustc(dagq) | 3539920 | 3646783488 | 28.17→20.72 |
| 20 | 2 | V4 | test-build | 2026-10-23T03:39:40Z | 2026-10-23T03:49:43Z | 603 | 0 | 4197712 | rustc(dagq) | 5862800 | 4338647040 | 20.72→24.33 |
| 21 | 2 | V3 | build | 2026-10-23T03:49:45Z | 2026-10-23T03:55:47Z | 362 | 0 | 4090496 | rustc(dagq) | 4139360 | 4219109376 | 23.82→24.92 |
| 22 | 2 | V3 | test-build | 2026-10-23T03:55:48Z | 2026-10-23T04:05:17Z | 569 | 0 | 3779344 | rustc(dagq) | 5680800 | 3944546304 | 24.92→26.35 |
| 23 | 2 | V2 | build | 2026-10-23T04:05:19Z | 2026-10-23T04:06:51Z | 92 | 0 | 3811376 | rustc(dagq) | 3860160 | 4065198080 | 26.35→9.77 |
| 24 | 2 | V2 | test-build | 2026-10-23T04:06:51Z | 2026-10-23T04:09:41Z | 170 | 0 | 3887888 | rustc(dagq) | 5943440 | 4082630656 | 9.77→4.05 |
| 25 | 2 | V1 | build | 2026-10-23T04:09:42Z | 2026-10-23T04:11:50Z | 128 | 0 | 2832576 | rustc(dagq) | 2856368 | 2916466688 | 4.05→4.08 |
| 26 | 2 | V1 | test-build | 2026-10-23T04:11:50Z | 2026-10-23T04:16:50Z | 300 | 0 | 4203488 | rustc(dagq) | 4226304 | 4424466432 | 4.08→4.33 |
| 27 | 2 | V0 | build | 2026-10-23T04:16:54Z | 2026-10-23T04:18:41Z | 107 | 0 | 3057152 | rustc(dagq) | 3080768 | 3282272256 | 4.39→7.48 |
| 28 | 2 | V0 | test-build | 2026-10-23T04:18:41Z | 2026-10-23T04:22:35Z | 234 | 0 | 3444688 | rustc(dagq) | 6928752 | 3554787328 | 7.48→17.11 |
| 29 | 1 | T6 | unit | 2026-10-23T04:22:35Z | 2026-10-23T04:23:16Z | 41 | 0 | 277248 | dagq-ce2f7223bf48f2d0 | 393024 | 283901952 | 17.11→13.08 |
| 30 | 1 | T2 | unit | 2026-10-23T04:23:16Z | 2026-10-23T04:24:48Z | 92 | 0 | 240592 | dagq-ce2f7223bf48f2d0 | 264000 | 248004608 | 13.08→8.47 |
| 31 | 2 | T2 | unit | 2026-10-23T04:24:48Z | 2026-10-23T04:26:03Z | 75 | 0 | 232432 | dagq-ce2f7223bf48f2d0 | 283120 | 241385472 | 8.47→6.37 |
| 32 | 2 | T6 | unit | 2026-10-23T04:26:03Z | 2026-10-23T04:26:34Z | 31 | 0 | 253872 | dagq-ce2f7223bf48f2d0 | 342560 | 261013504 | 6.37→6.45 |

### 集計

peak は 2 周の大きい方、時間は 2 周の平均。
比は同じ対象の基準（(i)・(ii) は V0、(iii) は T6）に対する値で単位なし。
MiB は KiB ÷ 1024 を丸めた読みやすさのための値。

| 対象 | variant | 1 process の peak（KiB） | MiB | 合計の peak（KiB） | MiB | time -l の max RSS の大きい方（byte） | 時間の平均（秒） | 1 process の peak ÷ 基準 | 合計の peak ÷ 基準 | 時間 ÷ 基準 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| (i) cargo build --locked --lib | V0 | 4159728 | 4,062 | 4209424 | 4,111 | 4293607424 | 219.5 | 1.00 | 1.00 | 1.00 |
| (i) cargo build --locked --lib | V1 | 3632976 | 3,548 | 3681936 | 3,596 | 3764027392 | 199.5 | 0.87 | 0.87 | 0.91 |
| (i) cargo build --locked --lib | V2 | 3811376 | 3,722 | 3860160 | 3,770 | 4065198080 | 102.5 | 0.92 | 0.92 | 0.47 |
| (i) cargo build --locked --lib | V3 | 4090496 | 3,995 | 4139360 | 4,042 | 4219109376 | 271.0 | 0.98 | 0.98 | 1.23 |
| (i) cargo build --locked --lib | V4 | 4121056 | 4,024 | 4169552 | 4,072 | 4295852032 | 206.5 | 0.99 | 0.99 | 0.94 |
| (i) cargo build --locked --lib | V5 | 3439136 | 3,359 | 3475184 | 3,394 | 3623813120 | 117.5 | 0.83 | 0.83 | 0.54 |
| (i) cargo build --locked --lib | V6 | 3496592 | 3,415 | 3544928 | 3,462 | 3685859328 | 95.0 | 0.84 | 0.84 | 0.43 |
| (ii) cargo test --locked --no-run | V0 | 5537088 | 5,407 | 7381504 | 7,208 | 5755600896 | 284.0 | 1.00 | 1.00 | 1.00 |
| (ii) cargo test --locked --no-run | V1 | 5518512 | 5,389 | 5567632 | 5,437 | 5801623552 | 359.5 | 1.00 | 0.75 | 1.27 |
| (ii) cargo test --locked --no-run | V2 | 5098480 | 4,979 | 7681984 | 7,502 | 5246582784 | 261.5 | 0.92 | 1.04 | 0.92 |
| (ii) cargo test --locked --no-run | V3 | 5409248 | 5,282 | 7010128 | 6,846 | 5568086016 | 431.0 | 0.98 | 0.95 | 1.52 |
| (ii) cargo test --locked --no-run | V4 | 5221040 | 5,099 | 7122448 | 6,956 | 5418418176 | 407.5 | 0.94 | 0.96 | 1.43 |
| (ii) cargo test --locked --no-run | V5 | 3793536 | 3,705 | 6489296 | 6,337 | 3904716800 | 200.5 | 0.69 | 0.88 | 0.71 |
| (ii) cargo test --locked --no-run | V6 | 4894832 | 4,780 | 6900960 | 6,739 | 5104975872 | 204.0 | 0.88 | 0.93 | 0.72 |
| (iii) cargo nextest run --locked --lib --bins | T6 | 277248 | 271 | 393024 | 384 | 283901952 | 36.0 | 1.00 | 1.00 | 1.00 |
| (iii) cargo nextest run --locked --lib --bins | T2 | 240592 | 235 | 283120 | 276 | 248004608 | 83.5 | 0.87 | 0.72 | 2.32 |


## 下げる手の効き

比は「variant の peak ÷ V0 の peak」（同じ対象、どちらも 2 周の大きい方、単位なし）と「variant の時間 ÷ V0 の時間」（2 周の平均）で、上の集計の表の右の 3 列。
(iii) は T6 を基準にした。

まず揺れの大きさ: 同じ V0 でも 2 周目の値は 1 周目の 0.73 倍（(i) の 1 process）、0.62 倍（(ii) の 1 process）、0.94 倍（(ii) の合計）だった。
時間も load（1 分）が 4〜28 の間で動いたので、1 process の peak の比で V0 の 2 周の差（0.27〜0.38）より小さい差は、この測り方では揺れと分けられない。

| 手 | (i) の peak の比 | (ii) の 1 process / 合計の peak の比 | 時間の比（(i) / (ii)） | 判定 |
| --- | ---: | --- | --- | --- |
| V1 `CARGO_BUILD_JOBS=1` | 0.87 | 1.00 / 0.75 | 0.91 / 1.27 | 合計を下げる唯一の手で、仕組みから確か（rustc が同時に 1 本なので合計が 1 process の peak とほぼ同じになる）。(ii) は遅くなる |
| V2 `CARGO_BUILD_JOBS=2` | 0.92 | 0.92 / 1.04 | 0.47 / 0.92 | 合計は下がらない |
| V3 `CARGO_INCREMENTAL=0` | 0.98 | 0.98 / 0.95 | 1.23 / 1.52 | 効かない |
| V4 `CARGO_PROFILE_DEV_CODEGEN_UNITS=16` | 0.99 | 0.94 / 0.96 | 0.94 / 1.43 | 効かない |
| V5 `CARGO_PROFILE_DEV_CODEGEN_UNITS=64` | 0.83 | 0.69 / 0.88 | 0.54 / 0.71 | 1 周目は (i)・(ii) とも同じ周の V0 より小さく速いが、2 周目は (ii) の 1 process が V0 より大きく (i) が遅い。差は揺れの幅と重なる |
| V6 `CARGO_PROFILE_DEV_DEBUG=0` | 0.84 | 0.88 / 0.93 | 0.43 / 0.72 | peak は (i)・(ii) とも両方の周で同じ周の V0 より小さいが、時間は 2 周目の (ii) で V0 より遅い。差は揺れの幅と重なる |

- V5 を V0 と 4 周の交互の組で測り直した結果（判定 does_not_hold）は [codegen-units-spike](codegen-units-spike.md) にある。
- (iii) の T2 は 1 process の peak 0.87・合計 0.72 で時間 2.32 だった。
  実行の合計の peak は T6 でも 384 MiB で、build の 5％ほどなので、test の thread 数を下げてもメモリの上限は変わらない。
- 1 本の build と test のメモリを決めるのは (ii) の合計の peak（V0 で 7,208 MiB）で、(i) の 1 process（V0 で 4,062 MiB）ではない。
  V0 の (ii) の合計の peak の時点では、最大の process（`rustc(dagq)`）の他に木の中に 1 周目で約 2.0 GiB、2 周目で約 3.3 GiB があった（その process の名前は採っていない）。

### 採るものと置き場の推奨

- V5 の codegen-units = 64: container のスモークで V0 と並べて測り直し、(ii) の合計の peak が下がると確かめてから採る。
  置き場は `Cargo.toml` の `[profile.dev]` で、host・container・CI のどれにも効く（時間が短くなる向きなので host にも害が無い）。
  採るかは receipt の follow_up にした。
- V6 の debug = 0: 採らない。
  `line-tables-only` は落ちた test の backtrace の行番号のためで、揺れの幅と重なる差のために失うものが大きい。
- V1 の `CARGO_BUILD_JOBS=1`: host には採らない（host には cgroup の上限が無く、(ii) が 1.27 倍遅くなる）。
  container に上限いっぱいの container を 2 本入れる手としても足りない（下の「推奨」）。
  container のときだけ値を変える置き場は今は無く（host と container の run は同じ `dagq.toml` の `[run.env]` を読む。[Container runtime](../design/container-runtime.md#workerの実行)の「workerの実行」）、要るなら機能から足す。
- V2・V3・V4: 効かないので採らない。
- `Cargo.toml` と `dagq.toml` はこの測定では変えていない。

## container との比

note 297078 の container の rustc の anon-rss 3856512 KiB ÷ host の V0 の (i) の 1 process の peak（dagq の lib の rustc）4159728 KiB = 0.93（単位なし）。

この比は目安で、container の中の値を host の値から求める換算には使わない。

- macOS の RSS は共有の page を含み、Linux の anon-rss は匿名の page だけで、同じ量ではない。
- 測ったときの host の負荷も、container の machine の負荷（同時の compile が 2 本）も違う。
- 絶対値は container の runtime のスモーク（task 2183）で、container の中の peak として確かめる。

目安として、container の中の dagq の rustc 1 本は host とほぼ同じ大きさ（0.9 倍ほど）と読める。

## 推奨

[運用](../development/operations.md)の「containerのmachineの資源（`host.toml`）」に、この節の値を推奨として書いた。

- container の memory の上限: 8 GiB（8,192 MiB）。
  1 本の worker の turn の peak は、(ii) の合計の peak 7,208 MiB に Claude Code の process（測定の終わりに host の `ps` で 1 度見た claude の process 3 本の RSS は 199〜313 MiB。CSV には無い）を足して約 7,521 MiB（約 7.3 GiB）。
  1 本の verification の peak は、test の binary の compile（(ii) と同じ形で 7,208 MiB）で、その後の実行（(iii)、384 MiB）は compile と重ならない。
  8 GiB の上限には host の RSS で約 0.65 GiB の余裕が残る。
  container の中の値が host より大きいか小さいかは「container との比」のとおりこの測定から言えないので、余裕が足りるかはスモークで確かめる。
- container の cpus: 4。
  `[run.env]` の `CARGO_BUILD_JOBS=4` と同じにし、(ii) の V0 をそのまま上限の前提にする。
- machine: 6 CPU / 10 GiB。
  host の 16 GiB から macOS と host に残る制御側（supervisor・inbox・planner・headless の review の job。Claude Code の process は上で見た 1 本 199〜313 MiB）に約 6 GiB を残す。
  測っている間、host は既に swap を約 1.1 GiB 使っていたので、host に残す分を 6 GiB より減らさない（planner の出発点の案のまま）。
  CPU は 8 コアのうち 2 を macOS と制御側に残す。
- 上限いっぱいで同時に入る container の数: 1 本（10 GiB ÷ 8 GiB）。
  `[supervisor]` の parallel = 3 の worker と verification の 4 本の container は、上限の合計（32 GiB）が machine を超える。
  peak が重ならない間は動くが、2 本の (ii) の peak が重なると machine 全体の OOM killer が働く（note 297078 と同じ形）。
  測った下げる手では 2 本は入らない（V1 でも (ii) の合計 5,437 MiB に Claude Code の 313 MiB を足すと約 5.6 GiB で、2 本で 10 GiB を超える）。
  container のときの並列数を決めることは receipt の follow_up にした。

## 測定の出力の区分と作り直せる範囲

- `build-memory/measure.sh`・`build-memory/summarize.py`・`build-memory/samples.csv`・`build-memory/runs.csv` は、この 1 回きりの測定の材料で、SSOT でも他の仕組みが読む共有の記録でもない。
  設計書（[計測](../design/measurement.md)の「SSOTとビュー」を含む）には足さない。
- この文書の「表」は `python3 -I docs/plans/build-memory/summarize.py` で `runs.csv` から作り直せる。
- `runs.csv` と `samples.csv` は、同じ commit で `sh docs/plans/build-memory/measure.sh all` を流し直すと作れるが、host の load で値は同じにならない。
  cargo の出力と `time -l` の出力は `$TMPDIR` の下に置き、commit していない。
