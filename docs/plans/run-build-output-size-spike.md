---
id: plan-run-build-output-size-spike
type: plan
title: スパイク：run の worktree の target（1 本 12〜13G）の内訳と、着地待ちの間の build の出力の扱い・cargo の設定で減らせる量と、その時間の代償
status: completed
created: 2026-10-26
owners:
  - hisamekms
tags:
  - planning
  - performance
  - measurement
related:
  - plan-build-memory
  - plan-codegen-units-spike
  - design-supervisor-lifecycle-run-worktrees
  - design-supervisor-lifecycle-claim-hold
  - development-local-checks
---

# スパイク：run の target の内訳と減らし方

問いは goal 224 の (d)：run の worktree の target が 1 本 12〜13G になる内訳と、どこを減らせば効くか、その代償（着地の検証の時間・sccache の hit・関門）。
問いごとの判定は (1) holds・(2) holds・(3) holds・(4) does_not_hold（下の「問いごとの判定と根拠」）。

要点：

- target の大半は workspace の crate（dagq）の incremental で、着地の検証の後の 10.3G（breakdown の値。同じ条件の 2・3 回目を `du -sk` で測ると 8.3〜8.9G）のうち 7.3G。deps 2.9G のうち 1.8G は macOS の既定の split-debuginfo（unpacked）が残す `.o`。依存の crate の出力は 0.14G しかない。
- build し直した crate の incremental の dir には、前の session が次の build まで残る。問い (3) の C で測ると、着地の検証の後の incremental の約 4 割（2.4G）はもう読まれない前の session だった。
- 着地の検証は run の incremental を使い直す。消すと build は約 1.8〜2.2 倍に延びる。test binary だけを消しても延びない。
- `CARGO_INCREMENTAL=0` は着地の検証の後の target を 2.2G（−79%）にするが、src の 1 file の更新時刻だけを変えた（`touch`、中身は同じ）後の build し直しが 22 秒から 93 秒に延びた。中身を変えた編集の後の代償は測っていない。`debug = 0` は −30% で、1 回の測定では時間は延びなかったが、backtrace の frame から file:line が消える。`split-debuginfo = "packed"` は −9%。

## 環境

| 項目 | 値 |
|---|---|
| 対象の commit | `6aabe6c2a`（worker の手元の検証。rebase の前の run の commit に見立てた）と `2441d29b7`（着地の検証。rebase の後の main に見立てた。差分は `0ebc13d14` の src 10 file・tests 2 file） |
| rustc / cargo | 1.98.1（48a229cea 2026-09-01）/ 1.98.1（797e8a9bc 2026-08-05） |
| sccache | 0.18.0（run の dagq-rustc-wrapper 経由。server は host の全 run が共有） |
| cargo-nextest | 0.9.146 |
| host | Apple M1・8 コア・16 GB、macOS 14.7.4。APFS の空き 41〜43 GiB |
| env | この run の worker の `[run.env]` のまま（`CARGO_BUILD_JOBS=4`・`RUST_TEST_THREADS=6`・`NEXTEST_TEST_THREADS=6`・sccache）。TMPDIR は run の一時ファイルの dir |
| provider | Claude の非対話の worker（この run）。打ったものは全て worker の turn の中、自分の worktree の target |
| 同時の load | 本番の他の run が同時に build していた。load average は 6〜29 で揺れ、時間の値は load の影響が大きい（各行に load を添えた） |

着地の検証の test は、全ての条件で同じ 8 本が落ちた（3505 passed・8 failed）。
unit test の 6 本は Unix socket の path が長すぎること（`path must be shorter than SUN_LEN`）と、queue の dir の上の link で落ちた。IT の 2 本は `runtime_sweep` の scratchpad の掃除で落ちた。
どれも TMPDIR が run の dir の下（長い path）にあることによる失敗で、build の大きさと時間の測定には関わらない。

## 打ったもの

測定の script は run の TMPDIR に置き、commit していない。

- `step.sh LABEL cmd…`：cmd を流して秒数、前後の `sccache --show-stats --stats-format json` の cache_hits と cache_misses の差、`sysctl vm.loadavg` を記録する
- `breakdown.py target`：target を directory ごとに分け、`target/debug/deps` は crate と拡張子ごとに、`target/debug/incremental` は crate ごとに `st_blocks` を足す。hard link を file ごとに数えるので、incremental を約 0.2G 多く数える。重なりを除いた値は `du -sk` で別に測った
- worker の手元の検証（`6aabe6c2a`、空の target から）：
  ```sh
  cargo fmt --all --check
  cargo clippy --locked --all-targets -- -D warnings
  cargo test --locked --lib domain::disk
  cargo test --locked --test it runtime_disk
  ```
- 着地の検証（`2441d29b7`）：`LANDING_IT_CACHE=$TMPDIR/... DAGQ_LANDING_BASE=$(git rev-parse HEAD~2) sh scripts/landing-it.sh`。IT は 94 の filter に絞られ、`cargo nextest run` は 3513 本を流した。対応表の cache は host のものに触れないよう run の TMPDIR に置いた
- 問い (2)：worker の手元の検証の後の target を `cp -cRp` で写し取り（APFS の clone）、条件ごとに戻して消してから着地の検証を流した。消すものは「全部」・`target/debug/incremental`・`target/debug/deps` の workspace の test と bin の実行 file（`dagq-<hash>`・`it-<hash>`・`e2e-<hash>`・`plugin-<hash>`）と `target/debug/dagq`
- 問い (3)：env で設定を変え、空の target から worker の手元の検証と着地の検証を通して流した（Cargo.toml は変えていない）。A は `CARGO_INCREMENTAL=0`、B は `CARGO_PROFILE_DEV_SPLIT_DEBUGINFO=packed`、C は `CARGO_PROFILE_DEV_DEBUG=0`。A と既定（incremental あり）では、`touch src/domain/disk.rs`（更新時刻だけを変え、中身は変えない）の後の `cargo test --locked --lib --no-run` の時間も測った
- backtrace の確かめ：小さな panic の program を `-C debuginfo=line-tables-only` と `split-debuginfo` の各値、`-C debuginfo=0` で build し、`RUST_BACKTRACE=1` の出力の file:line を見た
- 問い (4)：`dagq events --kind build_outputs_removed --kind scratchpad_removed --kind run_tmp_removed --full` を全件読んだ（2026-09-26〜10-25 の 2565 件）。`src/domain/disk.rs` の `run_size` と `src/application/mod.rs` の `recent_run_sizes` を読んだ

## 問いごとの判定と根拠

### (1) 内訳：holds

内訳を測れた。単位は GiB（`breakdown.py`）。

| 時点 | 合計 | deps | incremental | build | .fingerprint | target/debug の直下 | nextest |
|---|---|---|---|---|---|---|---|
| clippy の後 | 1.08 | 0.10 | 0.95 | 0.02 | 0.00 | 0 | なし |
| `test --lib` の後 | 3.24 | 0.77 | 2.45 | 0.03 | 0.00 | 0 | なし |
| worker の手元の検証の後（`test --test it` の後） | 6.25 | 1.98 | 4.11 | 0.03 | 0.00 | 0.13 | なし |
| 着地の検証の後 | 10.31 | 2.89 | 7.26 | 0.02 | 0.00 | 0.13 | なし |

着地の検証の後の deps と incremental の中身：

| もの | GiB |
|---|---|
| incremental：dagq の crate（7 dir。dir ごとの `du -sk` は hard link を重ねて数えず、大きい順に 2.80・2.16・0.42・0.38・0.07・0.04・0.01、合計 5.88） | 6.22 |
| incremental：tests/it（2 dir） | 0.99 |
| incremental：tests/e2e・tests/plugin・build script | 0.05 |
| deps：workspace の `.o`（2420 file。split-debuginfo が unpacked の既定で残る） | 1.76 |
| deps：`libdagq-*.rlib` | 0.41 |
| deps：unit test の binary（`dagq-<hash>`、3 本） | 0.30 |
| deps：tests/it の binary | 0.17 |
| deps：dagq の `.rmeta`（clippy の check を含む） | 0.10 |
| deps：tests/e2e・tests/plugin の binary | 0.02 |
| deps：依存の crate の rlib・rmeta・dylib（184 file） | 0.14 |
| `target/debug/dagq`（bin） | 0.13 |

読み取り：

- clippy（`--all-targets` の check）が足すのは約 1.0G（dagq の check の incremental 0.83G と tests/it 0.11G）。check の metadata で、build の rlib とは別の出力として残る（cargo の設定で重ねられるかは試していない）。
- test binary の数は効かない。tests/it は既に 1 本の binary で、tests/e2e と tests/plugin は incremental と binary を合わせて 0.06G。tests/ を統合しても減る量は 0.1G に届かない。
- 依存の crate の出力は 0.14G で、sccache が既に run の間で共有している。target の大きさは、run ごとに中身が違う workspace の crate が作る。
- 着地の検証は incremental を 4.1G から 7.3G に増やした。build し直した crate の dir（dagq の 2 dir と tests/it）には、worker の build の session が着地の検証の session の横に残る。dir の中の前の session は rustc が次にその crate を build するときまで消さない。session ごとの大きさは問い (3) の C でだけ測った。C の着地の検証の後では、incremental の 5.97G（`du -sk`）のうち新しい session は 3.59G で、前の session が 2.38G を占めていた。
- nextest は target に何も書かない（`target/nextest` は無い）。build script の出力と .fingerprint は合わせて 0.02G。

### (2) 着地の検証は run の target を使い直すか：holds

着地の検証のコマンドの時間（秒）。「build」は cargo の `Finished` の時間、「合計」は landing-it.sh の全体。「後」は着地の検証の後の target の `du -sk`（GiB）。

| 条件 | 回 | build | test 段 | 合計 | 消す前 → 後 | load（1 分） |
|---|---|---|---|---|---|---|
| target を残す | 1 | 130 | 108 | 255 | 6.25 → 10.31（breakdown） | 16.1 |
| target を残す | 2 | 141 | 173 | 388 | 6.12 → 8.33 | 24.8 |
| target を残す | 3 | 75 | 99 | 181 | 6.12 → 8.90 | 12.5 |
| 全部消す | 1 | 371 | 151 | 531 | 0 → 4.44 | 29.0 |
| 全部消す | 2 | 207 | 93 | 306 | 0 → 4.43 | 11.7 |
| incremental だけ消す | 1 | 303 | 119 | 431 | 2.11 → 5.33 | 17.3 |
| incremental だけ消す | 2 | 157 | 103 | 266 | 2.11 → 5.31 | 16.1 |
| test binary だけ消す | 1 | 61 | 93 | 160 | 5.54 → 8.87 | 12.8 |
| test binary だけ消す | 2 | 90 | 115 | 212 | 5.54 → 8.87 | 18.2 |

残すの 2 回目は load が 24.8 で 1 回目と食い違ったので、3 回目を 1 回だけ足した。

判定の根拠：

- build の時間は、残す（75〜141 秒）より incremental だけ消す（157〜303 秒）と全部消す（207〜371 秒）の方が長く、範囲が重ならない。rebase で dagq の crate は compile し直しになるが、incremental の前の session から codegen を使い直しているので、消すと着地の検証の build が延びる（残すの中央値 130 秒に対し、各条件の 2 回の平均で incremental だけ消すと約 1.8 倍、全部消すと約 2.2 倍）。
- incremental だけ消すのと全部消すのの差は、依存の crate を sccache から戻す分（約 50〜70 秒）だけ。消して空ける量は、incremental だけなら 4.1G、全部なら 6.3G（worker の手元の検証の後の値）。
- test binary だけ消しても build は延びない（61〜90 秒）。test binary は着地の検証で link し直すので、使い直されていない。空く量は 0.6G と小さい。
- 時間の揺れは load で大きく、同じ条件の 2 回で 1.5〜1.9 倍の差がある。sccache の hit と miss の差は host の全 run を合わせた server の数で、この run だけの値は取れなかった（全部消すの 2 回は hit 66・miss 0 で、依存の crate は全て hit したと読める）。

task 3437（空き容量の掃除で着地待ちの run の build の出力を消す）の代償：消された run の着地の検証の build が、全部消すと残すのより約 +2.5 分（+66〜+296 秒）延び、incremental だけなら約 +1.7 分延びる。

### (3) cargo の設定で減らせる量と代償：holds

空の target から worker の手元の検証（clippy・`test --lib`・`test --test it`）と着地の検証を通して 1 回ずつ。既定は問い (1) と問い (2) の「残す」の値。

| 案 | worker の手元の検証の後 | 着地の検証の後 | worker の 3 step の秒 | 着地の検証の build / 合計の秒 |
|---|---|---|---|---|
| 既定 | 6.25 | 10.31（du 8.3〜8.9） | 109 + 139 + 144 = 392 | 75〜141 / 181〜388 |
| A `CARGO_INCREMENTAL=0` | 2.11 | 2.16 | 69 + 122 + 289 = 480 | 171 / 259 |
| B `split-debuginfo = "packed"` | 6.12 | 9.42 | 148 + 193 + 171 = 512 | 78 / 192 |
| C `debug = 0` | 4.57 | 7.27 | 170 + 131 + 120 = 421 | 64 / 226 |

sccache の hit と miss の step ごとの差（前後の `sccache --show-stats` の差）。server は host の全 run が共有するので、同時に動いた他の run の compile も入る。この run だけの値ではない。

| 案 | clippy | `test --lib` | `test --test it` | 着地の検証 |
|---|---|---|---|---|
| 既定 | hit 60・miss 7 | hit 45・miss 7 | hit 59・miss 7 | 残すの 3 回で hit 46・miss 7、hit 0・miss 0、hit 53・miss 0 |
| A | hit 0・miss 67 | hit 0・miss 52 | hit 0・miss 1 | hit 0・miss 1 |
| B | hit 0・miss 67 | hit 60・miss 59 | hit 66・miss 0 | hit 2・miss 0 |
| C | hit 0・miss 67 | hit 0・miss 52 | hit 0・miss 0 | hit 35・miss 1 |

src の 1 file の更新時刻だけを変えた（`touch`、中身は同じ）後の `cargo test --locked --lib --no-run`：既定（incremental あり）22 秒、A 93 秒（load 6〜7）。中身を変えた編集の後は測っていない。

案ごとの読み取り：

- A：incremental が無くなり、着地の検証の後の target が 2.2G（−79%）。着地の検証の build は 171 秒で、既定の残す（75〜141 秒）より長いが、incremental を消したとき（157〜303 秒）と同じ程度。代償は worker の繰り返しの build。測ったのは中身の変わらない再 build（`touch` の後）で、既定 22 秒・A 93 秒だった。A は incremental を持たないので、中身を変えた編集でも dagq の crate を毎回まるごと compile し、93 秒前後かかると見られる。既定の側は編集の大きさで 22 秒より延びるので、編集の後の比は 4.2 倍より小さい。編集の後の値は測っておらず、比は結論にしない。sccache は incremental の無い workspace の crate も cache できる対象になるはずだが、`test --lib` の miss 52 は C でも出ており、server 全体の数なので A の効きとは分けられなかった。run ごとに src が違うので、run の間の hit は増えない見込み（測っていない）。
- B：deps の `.o` 1.76G が `.dSYM` 0.81G に替わり、−0.9G（−9%）。dsymutil の時間は load の揺れに埋もれて見えなかった。backtrace の file:line は残る。
- C：`.o` と line table が無くなり、−3.0G（−30%）。build の時間は 1 回の測定（load 7〜23）では延びなかった。代償は、panic の位置（`panicked at file:line`）は残るが、`RUST_BACKTRACE=1` の frame から file:line が消えること。小さな program で確かめた。Cargo.toml の `[profile.dev]` のコメントは backtrace の file:line を残すために line-tables-only を置くと書いており、C はその目的を捨てる。macOS の `split-debuginfo = "off"` も同じく frame の file:line が消えた。
- env で入れる案（A・B・C とも）は、最初の build で依存の crate も sccache の miss になった（miss 67）。sccache は `CARGO_` で始まる env を鍵に入れるためと読める。`[run.env]` に入れると全ての run が同じ値になるので、最初の 1 回の後はまた hit する見込み。Cargo.toml の `[profile.dev]` に入れるなら、依存の crate は既に `debug = false` で鍵が変わらない見込み（どちらも測っていない）。
- 関門への影響：`[run.env]` に置く案は host の run の build だけに効き、CI の `cargo llvm-cov` には届かない。Cargo.toml の `[profile.dev]` に置くと CI の test と coverage の build にも効く。`-C instrument-coverage` は debuginfo に依らないので、C で行カバレッジが変わるとは考えにくいが、llvm-cov は worker が流さない規則なので測っていない。
- clippy と build の出力の重なり：clippy の check の出力（約 1.0G）は build の rlib と別のもので、cargo の設定で重ねる手は無い。
- `CARGO_TARGET_DIR` を run の間で共有する案：覆す根拠は無い。共有で減るのは依存の crate の出力（0.14G）だけで、大きさの 9 割以上は run ごとに中身が違う dagq の crate の incremental・`.o`・test binary。共有すると run どうしがそれを上書きし合う。sccache の鍵と関門の誤りという `[run.env]` のコメントの理由に、得るものが釣り合わない。

測らなかった案：tests/ の統合（問い (1) の値から効かないと分かった）と、`CARGO_INCREMENTAL=0` を着地の検証だけに置く案（上限の 3 案を超えるため）。

### (4) 直近の run の大きさの見積もりは過大か：does_not_hold

見積もり（`run_size`）は、kind ごとに最新 20 件の event の bytes の最大を 3 つ足す（build の出力・scratchpad・run の TMPDIR）。本番の queue の events から同じ式で計算した値：

| 時点（UTC） | 見積もり | build の出力の最大 | scratchpad の最大 | run TMPDIR の最大 |
|---|---|---|---|---|
| 2026-10-20 00:00 | 11.77 | 11.31 | 0.00 | 0.46 |
| 2026-10-23 00:00 | 13.36 | 12.90 | 0.00 | 0.46 |
| 2026-10-25 07:00〜12:00 | 15.92 | 15.45 | 0.00 | 0.46 |
| 2026-10-25 19:00 | 14.74 | 14.28 | 0.00 | 0.46 |

`build_outputs_removed` の bytes（666 件、2026-09-26〜10-25）：中央値 5.38G・90 パーセンタイル 10.39G・最大 16.08G。2026-10-15 以降の reason ごとに見ると、`awaiting_answer` は 246 件で中央値 7.72G・最大 16.08G、`disk_space` は 39 件で中央値 7.00G・最大 13.38G、`run_ended` は 15 件で中央値 8.70G・最大 13.11G。
`scratchpad_removed`（1839 件）は 90 パーセンタイルでも 0.00G、最大 1.97G（2026-10-12）。`run_tmp_removed`（60 件、Codex の worker だけ）は最大 0.46G（2026-10-07）。

判定の根拠：

- 新しい run が 1 周（worker の手元の検証と着地の検証を 1 回ずつ）で書く量は、問い (1) で 6.3G（着地待ちの間）、8.3〜10.3G（着地の検証の後）。見積もりの 14.7〜15.9G はこの 1.4〜2.5 倍だが、本番では 12〜16G の run がある（10-25 の着地待ちの 3 本は 11.6〜14.3G）。1 周より大きい run は差し戻しや resume で build を繰り返したものと見ているが、run ごとの build の回数は確かめていない。見積もりは「最近の最大の run」を表すという式の目的どおりの値で、式のずれではない。
- 3 つの最大を別の run から足すことによる上乗せは、今は run TMPDIR の 0.46G だけ（scratchpad は 0）。run TMPDIR の event は少ないので、最新 20 件が約 3 週間前まで遡り、10-07 の 0.46G が残っている。量は小さく、Claude の worker も run TMPDIR を持つようになれば event が増えて入れ替わる。
- 同じ run の build の出力が `awaiting_answer` で何度も記録される。窓は kind ごとの最新 20 件の event で run の数ではないので、重複した event が窓を使い、別の run の大きな値を窓から押し出すことがある。上の時点の窓に入る run は 9〜12 本だった。窓を「最新 20 本の run の event」に替えて同じ時点を計算し直すと、build の出力の最大は 10-20 で 11.31→11.77G、10-25 19:00 で 14.28→15.45G になり、ほかの時点は変わらなかった。重複が動かすのは最大 1.2G で、向きは見積もりを小さくする側だけ。過大かという問いの判定は変わらない。窓を run の数で数え直すかは、見積もりを最大で 1.2G 上げる変更なので、後続の計画で決める。
- 見積もりと 1 周の run の差（4〜7G）の一部は、build し直した crate の前の incremental の session が残ることによる（問い (1) の C で約 2.4G）。それを消すようにすれば、記録される bytes と見積もりは式を変えずに下がる見込み。

## 後続の計画への推奨

採る案（効く順）：

1. build し直した crate の incremental の dir から、前の session を消す。C の着地の検証の後で約 2.4G（incremental の約 4 割）が空いた計算。既定でも dagq の大きい 2 dir が worker の手元の検証の後の 1.55G・1.22G から 2.80G・2.16G に増えており、同じことが起きていると見られる（既定の session ごとの大きさは測っていない）。差し戻しで build を繰り返した run ほど効く。rustc は crate ごとに一番新しい完了済みの session だけを読むので、時間の代償は無い見込み。ただしこの Spike では測っていない（上限の 3 案に入れなかった）。後続の task で、消した後の着地の検証の build の時間が「残す」と同じことを確かめる。消すのは build が動いていない run だけにする（goal 224 の制約）。
2. task 3437 の「空きが足りないときだけ消す」形を保つ。消すなら incremental だけでなく target 全部を消す：時間の差は約 1 分で、空く量は 4.1G に対して 6.3G。代償は消された run の着地の検証ごとに約 +2.5 分（+66〜+296 秒）。
3. `split-debuginfo = "packed"`：−0.9G（−9%）で、backtrace の file:line は残る。時間の代償は測定の揺れの中で見えなかった。Cargo.toml の `[profile.dev]` に置けば依存の crate の sccache の鍵は変わらない。効きは小さいので、1 と 2 の後で足りなければ採る。

採らない案と理由：

- 空きに依らず review の後に必ず target を消す：全ての着地の検証が約 +2.5 分延びる。着地の slot は 1 本なので、着地の数の上限が下がる（goal 224 の記述の 24h 平均 2.2 件/時の着地なら、slot の使用が 1 時間あたり約 5.5 分増える）。task 3437 の空きが足りないときだけの形で、同じ空きを代償の少ない run だけで得られる。
- `CARGO_INCREMENTAL=0` を worker を含めて `[run.env]` に置く：−79% と最も効くが、worker の build し直しが延びる（中身の変わらない再 build で 22→93 秒。A は編集でも crate をまるごと compile するので、93 秒前後かかると見られる）。`[run.env]` のコメントの理由（worker の繰り返しの build が遅くなる）がそのまま当たる。着地の検証だけに置く案は測っていない。上の 1 で足りなければ、後続の Spike で測る。
- `debug = 0`（または macOS の `split-debuginfo = "off"`）：−30% で 1 回の測定では時間は延びなかったが、`RUST_BACKTRACE=1` の frame から file:line が消え、Cargo.toml が line-tables-only を置く目的に反する。採るなら、test の失敗の調べやすさとの引き換えを人か planner が決める。
- tests/ の統合：test binary の分は 0.06G で、効かない。
- `CARGO_TARGET_DIR` の共有：共有で減るのは 0.14G で、sccache の鍵と関門の誤りの代償に見合わない。
- 見積もり（`run_size`）の式を直す：直すべきずれは無い。上の 1 が入れば、記録される bytes が下がって見積もりも下がる。

## 使った時間と回数

build と測定（cargo の build・test・clippy・landing-it.sh と、target の大きさの計測）に使ったのは合計約 95 分（step の秒の合計 5375 秒と、target の写し取り・大きさの計測・events の読み取り）。上限は 4 時間。

| 条件 | 回数 | 秒 |
|---|---|---|
| (1) worker の手元の検証（fmt・clippy・`test --lib`・`test --test it`） | 1 | 397 |
| (1)・(2) 残す | 3（2 回が食い違ったので 3 回目を足した） | 255 + 388 + 181 |
| (2) 全部消す | 2 | 531 + 306 |
| (2) incremental だけ消す | 2 | 431 + 266 |
| (2) test binary だけ消す | 2 | 160 + 212 |
| (3) A（worker と着地の検証、`touch` の後の build し直し） | 1 | 832 |
| (3) 既定の `touch` の後の build し直し（温める build を含む） | 1 | 65 |
| (3) B | 1 | 704 |
| (3) C | 1 | 647 |
| (4) events の読み取りと、窓を run の数で数えたときの計算し直し（保存した events から、build はしない） | 1 | 1 分未満 |

45 分を超えた測定は無く、打ち切った条件は無い。
