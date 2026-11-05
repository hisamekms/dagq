---
id: plan-it-coverage-map-linux-spike
type: plan
title: スパイク：Linux の runner で it-coverage-map の IT が安定して通り、partition で 1 回 15 分前後に収まり、表の作り方が Linux で同じに動くか
status: completed
created: 2026-11-03
owners:
  - hisamekms
tags:
  - planning
  - measurement
  - ci
related:
  - plan-coverage-at-landing
  - plan-landing-it-selection
---

# スパイク：it-coverage-map を Linux の runner で partition に分けて作れるか

問いは「Linux（ubuntu-24.04）の GitHub hosted runner で、it-coverage-map の対象の IT（nextest の filterset `kind(test) - binary(e2e)`）が安定して通るか。coverage つきで nextest の `--partition` により N 個の job に分けたとき、1 回が 15 分前後（build を含む）に収まるか。表の作り方（`scripts/it-coverage-map.sh` の 1 test 1 profile の方式）が Linux でも同じに動くか」。

読み取りと見積もりだけで答えた。workflow の起動・push・branch の作成はしていない。

判定は **holds**（条件つき。Linux だけで落ちる 1 本の直し task 3879 を先に置く。下の「判定」）。

## 環境

- 対象の commit: `8f580ea6600b2c2ae8c1ed4532b8a2fe34a21541`（この run の base）。CI の記録は main の `b8878ee21`〜`f3cc2e8fd`（下の run の表）。最新の成功した表は `8d0c08992969bcbcbe13e6df1b720c33cf811ab9` から作られた。
- 読んだ run:
  - ci.yml の main の push の 30 本（cancelled を除く、2026-11-01T04:44Z〜11-02T17:26Z）。id は 41274226937、41276565984、41281437393、41284605201、41289249914、41293610377、41297108933、41302293051、41307470258、41313557567、41316993598、41326080944、41329401485、41363372074、41367278324、41373316847、41376803241、41385588083、41386439534、41394471382、41408160129、41411053720、41425591438、41436651311、41446573640、41455661862、41463927348、41478129283、41485007365、41491162781。
  - it-coverage-map.yml の最新の成功 41358083984（schedule、2026-11-01T22:37Z、`8d0c08992`、macos-26-arm64）。
  - 同じ commit の ci.yml 41329401485 の job 124926488135（macOS の coverage の job）と 124926488145（linux build / test）の log。
- 道具の版（log から）: rustc・cargo 1.99.1、cargo-nextest 0.9.150（d225b1daf 2026-10-26、両 OS）、cargo-llvm-cov 0.9.1（macOS の job。ci.yml の Linux の job は入れていない）。gh 2.103.3（この run の host、hisamekms で認証済み）。
- runner: ubuntu-24.04（public repository の標準は 4 vCPU）、macos-26-arm64（3 vCPU）。
- 調べた経路: この run の worker（Claude、headless）から gh で読んだ。host には何も入れていない。

## 打ったコマンド

状態を変えるコマンドは打っていない（`gh workflow run`・`git push`・branch の作成なし）。

```sh
gh run list --workflow ci.yml --branch main --limit 60 \
  --json databaseId,conclusion,status,headSha,createdAt,event   # cancelled を除き 30 本
gh run view <id> --json jobs                                     # 30 本の job と step の時刻
gh run download <id> -n nextest-junit-linux -D <tmp>/<id>/...    # 30 本
gh run download <id> -n nextest-junit-macos -D <tmp>/<id>/...    # 30 本
gh run list --workflow it-coverage-map.yml --limit 15 --json databaseId,conclusion,headSha,createdAt,event
gh run download 41358083984 -n it-coverage-map -D <tmp>/map
gh run view 41358083984 --log                                    # partition ごとの時刻
gh run view --job 124926488135 --log                             # macOS の coverage の job
gh run view --job 124926488145 --log                             # linux build / test
dagq search runtime_replan_park
dagq search an_answer_whose_send_meets_the_fence
dagq search auto_update_installs_each_runtime_landing
```

JUnit と表の集計は、`$TMPDIR` に置いた使い捨ての python（`xml.etree` で testcase の `failure`・`rerunFailure`・`flakyFailure` と `time` を読む）で行った。

## (a) 直近 30 本の linux build / test

30 本の内訳は、Linux の job が success 26、failure 3、skipped 1（41386439534、docs だけの差分で両 job が skipped）。

30 本ごとの結論（IT の本数は JUnit の `dagq::it`・`dagq::plugin` の skipped でない testcase の数。`…` の test の全名は下の集計の表）:

| run | commit | 時刻（UTC） | Linux の結論 | Linux の IT の本数 | Linux で落ちた test | macOS の結論 | macOS の IT の本数 | macOS で落ちた test |
|---|---|---|---|---|---|---|---|---|
| 41491162781 | `f3cc2e8fd` | 11-02 17:26 | success | 1923 | — | success | 1923 | — |
| 41485007365 | `f04b0a1a9` | 11-02 16:48 | success | 1923 | — | cancelled | — | — |
| 41478129283 | `ce0ed790b` | 11-02 16:06 | success | 1923 | — | success | 1923 | — |
| 41463927348 | `77dc759be` | 11-02 14:38 | failure | 1923 | `cli_version::auto_update…`（2 回とも失敗） | success | 1923 | — |
| 41455661862 | `9d290a175` | 11-02 13:45 | success | 1922 | — | success | 1922 | — |
| 41446573640 | `be9c918a1` | 11-02 12:40 | success | 1922 | — | success | 1922 | — |
| 41436651311 | `6ba79543e` | 11-02 11:24 | success | 1922 | — | success | 1922 | — |
| 41425591438 | `0e2cbb5c7` | 11-02 10:02 | success | 1922 | — | success | 1922 | — |
| 41411053720 | `713533f61` | 11-02 08:10 | success | 1922 | — | success | 1922 | — |
| 41408160129 | `517859fa5` | 11-02 07:43 | success | 1922 | — | success | 1922 | — |
| 41394471382 | `47f0fe073` | 11-02 05:31 | success | 1922 | — | success | 1922 | — |
| 41386439534 | `c82f3741f` | 11-02 04:04 | skipped | — | — | skipped | — | — |
| 41385588083 | `fa9f50080` | 11-02 03:54 | success | 1922 | — | success | 1922 | — |
| 41376803241 | `6feefe622` | 11-02 02:16 | success | 1922 | — | success | 1922 | — |
| 41373316847 | `81e9fdc06` | 11-02 01:35 | success | 1922 | — | failure | 1922 | `runtime_replan_park::an_answer…`（FLAKY） |
| 41367278324 | `7fe35c1e7` | 11-02 00:27 | success | 1922 | — | success | 1922 | — |
| 41363372074 | `be4db5f1d` | 11-01 23:41 | success | 1922 | — | success | 1922 | — |
| 41329401485 | `8d0c08992` | 11-01 17:02 | success | 1922 | — | success | 1922 | — |
| 41326080944 | `b731516f4` | 11-01 16:23 | success | 1922 | — | success | 1922 | — |
| 41316993598 | `3ea4d1cfd` | 11-01 14:35 | success | 1922 | — | success | 1922 | — |
| 41313557567 | `77caa9f33` | 11-01 13:55 | success | 1921 | — | success | 1921 | — |
| 41307470258 | `df7867a1e` | 11-01 12:38 | success | 1921 | — | success | 1921 | — |
| 41302293051 | `347d18b73` | 11-01 11:31 | failure | 1921 | `cli_version::auto_update…`（2 回とも失敗） | success | 1921 | — |
| 41297108933 | `47dc8b16d` | 11-01 10:21 | failure | 1921 | `cli_version::auto_update…`（FLAKY） | success | 1921 | — |
| 41293610377 | `923d73eb8` | 11-01 09:33 | success | 1921 | — | success | 1921 | — |
| 41289249914 | `7416a922b` | 11-01 09:31 | success | 1921 | — | success | — | — |
| 41284605201 | `e7654fc2b` | 11-01 07:23 | success | 1921 | — | success | 1921 | — |
| 41281437393 | `3751602b8` | 11-01 06:35 | success | 1921 | — | success | 1921 | — |
| 41276565984 | `2f4c35c44` | 11-01 05:20 | success | 1921 | — | success | 1921 | — |
| 41274226937 | `b8878ee21` | 11-01 04:44 | success | 1921 | — | success | 1921 | — |

失敗の 3 本は、どれも同じ 1 本の test だけで落ちた。41463927348（11-02）と 41302293051 は 2 回とも失敗（`failure`＋`rerunFailure`）、41297108933 は FLAKY（2 回目で通過、`flaky-result = "fail"`）だった。

test ごとの集計（Linux の job が走った 29 本と、同じ実行の macOS の job の JUnit を比べた）:

| test | Linux で落ちた回数 | macOS で落ちた回数 | Linux だけか | 対応 |
|---|---|---|---|---|
| `cli_version::auto_update_installs_each_runtime_landing_and_puts_a_broken_build_back` | 3 / 29（うち FLAKY 1） | 0 / 27 | Linux だけ | task 3879（ready、base には未着地）。finding 1544（planner の読み取り）。前の別の競合は task 3659 が 10/29 に直した |
| `runtime_replan_park::an_answer_whose_send_meets_the_fence_is_delivered_after_the_continuation` | 0 / 29 | 1 / 27（FLAKY、41373316847） | macOS だけ | task 3997（ready） |

- 失敗の中身（41463927348）は `tests/it/cli_version.rs:967` の `assert_eq!`。壊れた build の `update_failed` の `stage` が `install` の想定に対し `build` だった。task 3879 の題がこの事象そのものである。
- macOS の JUnit が無い run は 3 本ある。41485007365 は macOS の job が cancelled、41289249914 は macOS の artifact が見つからず、41386439534 は skipped。
- IT の本数は commit で増え、29 本の Linux の JUnit で 1,921 本が 10 run、1,922 本が 15 run、1,923 本が 4 run だった。Linux と macOS の JUnit が両方ある 27 run では、どれも同じ run の Linux と macOS で IT の名前の集合が一致した（Linux だけ・macOS だけにある IT は無い）。表の commit `8d0c08992`（41329401485）では、Linux・macOS・表の 3 つが同じ 1,922 本だった。
- 表への影響: `it-coverage-map.sh` は落ちた test を `status: failed` で表に残し、覆ったファイルも保つ（exit 0）。そのため、1 本の Linux だけの失敗は表の作り直しを止めない。

## (b) N=4・6・8 の 1 job の所要

### 測った値

| 量 | 値 | 出どころ |
|---|---|---|
| Linux（plain）の IT の test 時間の和 S_plain | 中央値 6,452 秒、p90 6,676、範囲 4,546〜7,052（29 本） | nextest-junit-linux の `dagq::it`・`dagq::plugin` の testcase の `time` の和 |
| Linux の `cargo build` の step | 中央値 86 秒（58〜99） | job の step の時刻 |
| Linux の test の binary の build | 150 秒（`Finished test profile ... in 2m 30s`） | 41329401485 の Linux の log |
| Linux の `cargo nextest run` の step（unit＋IT、build を含む） | 中央値 1,919 秒（1,360〜2,061） | job の step の時刻 |
| macOS の coverage の CI（`cargo llvm-cov nextest`）の IT の和 | 8,025 秒（`8d0c08992`） | nextest-junit-macos |
| 表の job（coverage＋1 test 1 profile）の IT の和 | 9,640 秒（`8d0c08992`、1,922 本すべて passed） | it-coverage-map.json の `duration_secs` |
| 表の和 ÷ macOS の CI の和 | 1.10〜1.20（同じ commit の表と、前後 5 本の CI） | 上の 2 行 |
| 表の job の coverage つきの build | 627 秒（`Finished test profile ... in 10m 27s`） | 41358083984 の log |
| macOS の CI の coverage つきの test の build | 407 秒（`in 6m 47s`） | 124926488135 の log |
| 表の job の 8 partition の test の壁時計の和 | 3,264 秒（partition ごとに 311〜508 秒） | log の最後の PASS 行と `exported N tests` の行の時刻 |
| 同じく export の和 | 510 秒（partition ごとに 54〜89 秒） | 同上 |
| 表の job の Build the map の step の全体 | 4,427 秒（73.8 分） | step の時刻 |

- 並列の効率: 表の job の test の壁時計は 3,264 秒で、和 ÷ CPU 数（9,640 ÷ 3 = 3,213 秒）の 1.02 倍だった。そのため、IT の段は「和 ÷ (CPU 数 × 0.98)」と見る。
- export の単価: 510 秒 × 3 CPU ÷ 1,922 本 ≈ 0.80 CPU 秒/本。
- Linux の S_plain は二つの山に分かれる。6 本が 4,546〜4,839 秒、23 本が 5,504〜7,052 秒で、runner の CPU の当たり外れと見る。見積もりには中央値と p90 を使う。

### coverage の倍率 k の根拠

- macOS の coverage つきと plain の比: [着地の検証でcoverageの計測をやめたときの短縮の見積もり](coverage-at-landing.md)によると、host の test 段は計測の有無で見分けられるほど変わらない（A/B でも入れ替わる）。build は、plain が coverage つきの 0.14〜0.52 倍（warm）だった。
- 1 test 1 profile の上乗せ: 表 ÷ CI = 1.10〜1.20。
- そこで中心の k = 1.0 × 1.2 = 1.2 とした。plain の test 段に 1 割の上乗せが隠れていた場合と、Linux の llvm-cov の計装が遅い場合を見込み、保守の k = 1.5 とした。
- S_cov（中心）= 6,452 × 1.2 = 7,742 秒。S_cov（保守）= 6,676（p90）× 1.5 = 10,014 秒。

### coverage つきの build の見積もり B

- Linux の plain の build は 86 + 150 ≈ 240 秒である。coverage-at-landing の比の逆数（1.9〜7 倍）をそのまま掛けると 460〜1,700 秒で、幅が広すぎる。
- macOS では、plain の Linux の test の build（150 秒）に対し、coverage つきが 407 秒（CI）と 627 秒（表の job、workspace の clean を含む）だった。CPU の数（3 対 4）を考えると、Linux の coverage つきは 2.5 倍前後と見る。
- 中心の B = 600 秒（240 × 2.5、計装した依存 crate の cache が当たるとき）。保守の B = 900 秒（cache が外れたとき）。

### 式と値

1 job の所要 T(N) = 準備 90 秒（checkout・toolchain・install・cache の復元と upload）＋ B ＋ IT(N) ＋ E(N)。

- IT(N) = S_cov ÷ (4 vCPU × N × 0.98) ＋ 50 秒（partition の最長の test の待ち。表の最長は 52 秒）
- E(N) = 1,922 ÷ N × 0.80 ÷ 4（保守は 1.5 倍）

| N | IT(N) 中心 / 保守 | E(N) 中心 / 保守 | T(N) 中心 | T(N) 保守 |
|---|---|---|---|---|
| 4 | 544 / 689 秒 | 96 / 144 秒 | 1,330 秒（22.2 分） | 1,823 秒（30.4 分） |
| 6 | 379 / 476 秒 | 64 / 96 秒 | 1,133 秒（18.9 分） | 1,562 秒（26.0 分） |
| 8 | 297 / 369 秒 | 48 / 72 秒 | 1,035 秒（17.3 分） | 1,431 秒（23.9 分） |

最後の合わせる job（N 個の artifact の download と assemble）は約 2 分と見る。1 回の壁時計は、N=8 の中心で約 19 分、保守で約 26 分になる。

- build が床になる。N を 8 より増やしても、縮むのは IT と export の分（N=8→12 で約 1.7 分）だけである。
- 15 分前後に寄せる手は B を縮めること。計装した依存 crate の cache が当たり、B が 400 秒なら、N=8 の 1 job は約 14 分になる。build を 1 job にまとめて test の binary を渡す形は、総 CPU は減らすが壁時計（build の後に partition）は縮めない。
- 1 時間ごとの周期: 保守の 26 分でも 60 分に収まる。cancel-in-progress を false にしても、待つのは次の 1 本だけで溜まらない。
- 推奨の N は **8**。N=6 との差は中心で約 1.6 分、保守で約 2.2 分である。public repository の Linux の runner は費用がかからず、同時 20 job の枠に収まる。`IT_COVERAGE_PARTITIONS` の既定とも同じ数なので、partition の分け方が今の表と変わらない。

## (c) 表の作り方で Linux で違うところ

`scripts/it-coverage-map.sh`・`scripts/it-coverage-map.py`、cargo-llvm-cov の README、nextest の target runner の扱いを読み、macOS の表の job の log と比べた。

| 項目 | macOS | Linux | 変更が要るか |
|---|---|---|---|
| target runner の環境変数 | `CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER`（log にある） | `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER` | 要らない。script が `rustc -vV` の host から名前を作る |
| `llvm-profdata`・`llvm-cov` の場所 | `<sysroot>/lib/rustlib/aarch64-apple-darwin/bin` | `<sysroot>/lib/rustlib/x86_64-unknown-linux-gnu/bin` | 要らない。`llvm_tool` が host から作る。`rust-toolchain.toml` の `llvm-tools-preview` は host の toolchain に入る |
| cargo-llvm-cov | 表の job が `taiki-e/install-action@cargo-llvm-cov` で入れる | ci.yml の Linux の job は入れていない | 新しい Linux の job に install の step を足す |
| object（`llvm-cov export` に渡す binary） | Mach-O。coverage map は `__LLVM_COV` の segment | ELF。`__llvm_covmap` の section | 要らない。test の binary（`binary.txt`）と nextest の list の `non-test-binaries` の path を渡すだけで、dSYM のような別ファイルは要らない |
| `LLVM_PROFILE_FILE` の `%1m`（pool）と `%p` | 使える | 使える（clang の profile runtime は Linux と Darwin で `%Nm` を持つ） | 要らない。spawn した dagq の process も変数を継ぐ |
| repository の root | `/Users/runner/work/dagq/dagq` | `/home/runner/work/dagq/dagq` | 要らない。`covered_files` が root からの相対 path にする。`IGNORE_FILENAME_REGEX` の `/.cargo/registry`・`/.rustup`・`/rustc/` も Linux の path に当たる |
| CPU の数 | 3 | 4 | 要らない。export の `--jobs` と nextest の並列度は CPU の数が既定 |
| partition の回し方 | 1 job で 1..N を順に回す | N job に matrix で 1 つずつ | 要る（OS に依らない）。1 partition だけを流して export まで行う mode と、N job の export（またはファイルの一覧）・JUnit・list を受けて `assemble` だけを行う mode。nextest の `hash:i/N` は test の名前の hash で決まり、job をまたいで同じ分け方になる |
| `cargo llvm-cov show-env --export-prefix` | 「`--sh` に改名」の warning（0.9.1） | 同じ | 任意。`--sh` に直すと将来の削除に備えられる |
| `cargo llvm-cov clean --workspace` | 「missing or invalid `CACHEDIR.TAG`」で失敗し、warning のまま進む（41358083984 の log） | rust-cache が復元した target で同じになる見込み | 任意。rust-cache は保存の前に workspace の crate を消すので、今は害が見えていない |
| rust-cache の key | 表の job は単独 | matrix の N job は同じ job id で key を分け合う | 要る。計装した依存の cache を 1 つの key で保存・共有し、`linux build / test` の plain の cache と混ぜない |
| disk の空き | macOS の CI は空ける step を持つ | 測っていない | 初回に測る。profraw は 1 partition 分（1,922 ÷ N 本）だけ |
| 対象の IT の集合 | 同じ run の Linux と同じ名前の集合（27 run で一致） | 同じ run の macOS と同じ名前の集合 | 要らない。tests の `#[cfg(target_os)]`・`cfg!(target_os)`（`runtime_sccache.rs`・`runtime_headless.rs`・`queue_broker.rs`・`runtime_support/thread_stacks.rs`）は helper や test の中の分岐で、test の有無は変えない |

Linux でだけ方式が壊れる箇所は見つからなかった。要る変更は matrix に分けること（OS に依らない）と、Linux の job の道具の install と cache の key である。

## (d) cfg(target_os) を使う src のファイルと Linux の表で欠ける関数

`grep -rln "target_os" src` の結果は 8 ファイルだった。`#[cfg(...)]` で使うのは adapters.rs・agent_dir.rs・setup_review.rs・queue_service.rs・launchd.rs（test だけ）で、`cfg!(...)` の分岐で使うのは claude_credential.rs・git_binary.rs・host_metrics.rs・launchd.rs である。task の文が挙げた 3 つより多い。

| ファイル | Linux の build に無い・走らないもの（macOS だけ） | Linux の表への影響 |
|---|---|---|
| `src/infrastructure/adapters.rs` | `process_cwd`（`proc_pidinfo`）と `process_executable`（`proc_pidpath`）の macOS 版。unit test の `proc_pidinfo_reads_a_childs_directory_and_none_for_a_pid_that_runs_nothing` | 同じファイルに `/proc` 版があり、呼ぶ側も同じなので、ファイルの結びつきは残る見込み。macOS 版を変えたときは、Linux 版を走らせた test が選ばれる |
| `src/infrastructure/agent_dir.rs` | unit test の中の `non_utf8` の作り方だけ | IT には影響しない |
| `src/infrastructure/claude_credential.rs` | 関数そのものは欠けない。macOS だけなのは host の keychain（`Keychain::of_host` が `Some` を返し `/usr/bin/security` を指すこと）で、Linux では `None` になる。そのため、`of_host` から keychain を受ける本番の 3 か所（`src/compose/observation.rs` の report の `claude_credential`、`ClaudeCredentialGate::of_queue`、`src/infrastructure/queue_broker.rs` の `SystemBrokerBackend`）では、Linux の test は keychain の無い分岐を通る。`impl KeychainLookup for Keychain` の `find` の実装は Linux の IT でも走る。`tests/it/queue_broker.rs` の `the_keychain_s_command_gets_the_item_by_its_arguments_and_gives_the_value_by_its_output` が、OS の除外なしに fake の `security` を渡した `Keychain` を作り、`read`・`report` から `find` を通すからである（Linux の JUnit 41329401485 で passed、macOS の表でも claude_credential.rs に結びつく） | ファイルは結びつき、`find` の変更はこの test を選ぶ。Linux の表で分からないのは、上の 3 か所で host の keychain を通る test（macOS で `of_host` が `Some` のときにだけ keychain の分岐を走る test）だけである |
| `src/infrastructure/git_binary.rs` | `resolve_shim` に `macos = false` を渡すので、`find_developer_git`（xcrun の shim の解決）が走らない | `git_executable` は走るのでファイルは結びつく |
| `src/infrastructure/host_metrics.rs` | `ps` の macOS の分岐（`comm=`） | Linux の分岐が同じ関数にあり、ファイルは結びつく |
| `src/infrastructure/launchd.rs` | `HAS_LAUNCHD = false` なので、`Launchctl::state`・`bootout` が早く返る。`install` は `ensure!(HAS_LAUNCHD)` で plist を書く前に失敗する。launchctl の起動（bootstrap・bootout・print）の部分が走らない | 早く返る行は走るのでファイルは結びつくが、結びつく test は macOS より少ない見込み（macOS の表では 297 本） |
| `src/infrastructure/queue_service.rs` | `sun_len` の設定、`peer_pid` の apple 版（`LOCAL_PEERPID`）、`peer_uid` の非 Linux 版（`getpeereid`） | Linux 版（`ucred`）が同じファイルにあり、ファイルは結びつく |
| `src/infrastructure/setup_review.rs` | errno の初期化の `__error`（macOS）。Linux は `__errno_location` | ファイルは結びつく |

- 表はファイルの単位なので、これらのファイルが Linux の表から消えることは無い見込みである。どのファイルも OS に依らない部分を持ち、呼ぶ側も同じだからである。欠けるのは、macOS だけの関数（または claude_credential.rs のように macOS だけで値が変わる分岐）を変えたときに、それを実際に走らせる test が分からないことである。
- そのため、goal の受け入れ (2) どおり、このファイルの変更では表が知らないファイルと同じ規則で IT を広げるのが安全である。検出は `#[cfg(target_os...)]` と `cfg!(target_os...)` の両方を数え（`target_vendor = "apple"` も）、今は 8 ファイルとする。

## 判定

**holds**（条件つき: Linux だけで落ちる test の直し task 3879 を先に着地させること）。

- 安定: 直近の 29 本で、Linux だけで落ちた test は 1 本だけ（3 回、10%）で、今の main ではまだ安定して通っていない。その 1 本には task 3879 が直しとして ready にあり、対応の無い Linux だけの失敗は無い。ほかの 1,920 本余りの IT は 29 本とも Linux で通った。安定は task 3879 の着地を条件として成り立つ（表が失敗の test も記録して作られることは、安定の根拠にしない）。
- 所要: N=8 で 1 job は中心 17 分、保守 24 分と見積もった。合わせる job を含む 1 回は約 19〜26 分である。15 分前後には build（約 10 分）が床として効く。計装した依存の cache が当たれば 15 分前後に寄るが、当たらなくても 1 時間ごとの周期に収まる。
- 方式: Linux で壊れる所は見つからなかった。target runner の変数名と LLVM の道具の path は host から作られ、object と `%1m` も Linux で同じに扱える。要る変更は OS に依らない matrix の分け方と、Linux の job の道具と cache である。

条件と限り:

- Linux で coverage つきの IT は実際には流していない。時間は plain の Linux と macOS の coverage の比からの見積もりである。
- 15 分の目標は、中心の見積もりでも 2〜4 分超える。
- disk の空きは測っていない。

## 後続への推奨

- 先に、Linux だけで落ちる `cli_version::auto_update_installs_each_runtime_landing_and_puts_a_broken_build_back` の直し（task 3879）を置く。goal 244 の Linux の並列の表の job の draft は task 3879 に依存させ、その着地の後に N=8 で submit する。cfg(target_os) のファイルの広げ方と入れた後の測定の draft も、表の job の後に置く。
- task 3879 の着地の後、ci.yml の linux build / test の数本と初回の表で、Linux で failed・flaky の test が無いかを見る。新しく Linux だけで落ちる test が出たら、その直しの task を同じく先に置く。
- 表の job には次を入れる。
  - partition を matrix にして 1 job に 1 つ（`hash:i/8`）を流し、各 job は export まで行う。
  - 合わせる job（name は今の「file -> IT test map」）は、artifact の受け渡しを export の JSON か覆ったファイルの一覧にし、`assemble` だけを行う。
  - Linux の job に cargo-llvm-cov を入れる。
  - rust-cache は計装した build 専用の共有 key にする。
  - `show-env --sh` に直す。
- cfg のファイルの検出は `#[cfg]` と `cfg!` と `target_vendor` を含め、今の 8 ファイルとする。
- 周期は 1 時間ごとでよい（保守でも 26 分）。入れた後の初回の数本で job ごとの build・IT・export の秒と disk の空きを記録し、この見積もりと比べる。1 job が 30 分を超えたら N を増やすより先に build の cache を直す（N を増やしても縮むのは IT と export の分だけ）。

## task 3879 の着地の後の確認

問いは「task 3879 の着地 commit を含む main の commit で、ci.yml の『linux build / test』job が、`cli_version::auto_update_installs_each_runtime_landing_and_puts_a_broken_build_back` を含めて Linux だけの失敗なく通るか」。

判定は **holds**。ただし、判定の範囲の外（後の 34 本）に Linux だけの失敗が 4 件ある（下の「判定の範囲の後の run（補足）」）。

### 環境

- task 3879 の着地 commit: `85f0c4e9c3a780824908d1d98ba3d82665b623c6`（2026-11-02T21:29:40Z）。その commit 自身の ci.yml の run 41528848805 は cancelled で、job が無い。
- 読んだ時刻: 2026-11-05T20:23Z（UTC）前後。この run の base は `0598dcf80`。開始の時点で対象が 3 本を超えていたので、待っていない。
- 道具の版（job の log から）: cargo-nextest 0.9.150（d225b1daf 2026-10-26）を対象の 10 本すべてで確かめた。runner は ubuntu-24.04。gh 2.103.3（この run の host）。
- nextest の設定（`.config/nextest.toml`）: retries は 1。CI の JUnit では、2 回目で通った test も flaky で失敗の扱いになり、`failure` と `flakyFailure` を持つ。2 回とも落ちた test は `failure` と `rerunFailure` を持つ。
- 調べた経路: この run の worker（Claude、headless）から gh で読んだ。`dagq search` は打てたが、`dagq ci failures` はこの run の経路で権限により拒まれたので、CI の失敗との対応は `dagq search` で調べた。

### 打ったコマンド

状態を変えるコマンドは打っていない（`gh workflow run`・`gh run rerun`・`git push`・branch の作成なし）。

```sh
git log --grep "Dagq-Task: 3879" --format='%H %cI %s'
git fetch origin main                                            # 手元の origin/main の更新だけ
gh run list --workflow ci.yml --branch main --event push --limit 100 \
  --json databaseId,headSha,status,conclusion,createdAt
git merge-base --is-ancestor 85f0c4e9c3a780824908d1d98ba3d82665b623c6 <head_sha>
gh run view <id> --json jobs                                     # job の結論と時刻
gh run view --job <job id> --log                                 # nextest の版と runner の image
gh api repos/hisamekms/dagq/actions/runs/<id>/artifacts          # artifact の有無と期限
gh run download <id> -n nextest-junit-linux -D <tmp>/<id>/linux
gh run download <id> -n nextest-junit-macos -D <tmp>/<id>/macos
dagq search the_spike_limit_defers_a_spike                       # 下の 4 本も同じ形で
```

JUnit の集計は、`$TMPDIR` に置いた使い捨ての python（`xml.etree` で testcase の子の要素を読む）で行った。

### 対象の run

対象は、`head_sha` が着地 commit を祖先に持つ push の run のうち、linux build / test が success か failure で完了したものの、古い方から 10 本である。途中の cancelled の run（job が無い）と、41589597163（linux build / test が skipped）は数えていない。時刻は UTC。「対象の test」は `cli_version::auto_update_installs_each_runtime_landing_and_puts_a_broken_build_back` である。macOS の結論は同じ run の job『fmt / test / clippy / coverage』の結論である。

| run | head_sha | run の時刻 | linux build / test の時刻 | Linux の結論 | macOS の結論 | 対象の test（Linux、retry の有無） | Linux だけの failed / flaky | 両 OS で落ちた test |
|---|---|---|---|---|---|---|---|---|
| 41533298296 | `adde2578c2bf510a8fbc999a6efe84d83f0f4644` | 11-02 21:59 | 11-02 22:45〜23:14 | success | success | passed、retry なし | 無い | 無い |
| 41540468881 | `773abf86d7410fe9a2cb28802abfa4b4c601138d` | 11-02 22:49 | 11-02 23:56〜00:25 | success | success | passed、retry なし | 無い | 無い |
| 41551493994 | `1aa44a1fbd610956cf40df3785487a491eaa6f03` | 11-03 00:15 | 11-03 01:04〜01:34 | success | success | passed、retry なし | 無い | 無い |
| 41562256414 | `1f2d8341a65a9a46ea498c176fcf21d588297647` | 11-03 01:49 | 11-03 02:10〜02:38 | failure | failure | passed、retry なし | 無い | 下の 4 本 |
| 41566167085 | `2dc2ec0329abb15b630da0f7a023f12dda9a36fa` | 11-03 02:26 | 11-03 03:12〜03:42 | failure | failure | passed、retry なし | 無い | 下の 4 本 |
| 41576691392 | `f29be15f83e72c4fdf47f67d9bbd109d8f26b1cd` | 11-03 04:11 | 11-03 04:15〜04:44 | failure | failure | passed、retry なし | 無い | 下の 4 本 |
| 41580153816 | `687f5e7861c59901d1a016bfd98cb19f6f3fdb42` | 11-03 04:47 | 11-03 05:14〜05:40 | failure | failure | passed、retry なし | 無い | 下の 4 本 |
| 41590923283 | `e91a56c75e0143a664bf935e58c071d25640bf99` | 11-03 06:36 | 11-03 06:36〜07:00 | failure | failure | passed、retry なし | 無い | 下の 4 本 |
| 41596229525 | `fbb5f4d6674ae4512745dc7e733c2e4961a55e23` | 11-03 07:25 | 11-03 07:37〜08:04 | failure | failure | passed、retry なし | 無い | 下の 4 本 |
| 41604256757 | `d9e7b3eaf375d381c94268ea2271c4d5e6c5d75f` | 11-03 08:34 | 11-03 08:40〜09:10 | failure | failure | passed、retry なし | 無い | 下の 4 本 |

両 OS で落ちた 4 本は次のとおり。どれも同じ run の Linux と macOS の両方で 2 回とも落ちたので、Linux だけの失敗ではない。この 4 本は task 4076 が直す。

- `actor_model::jobs_with_a_role_table_are_given_its_model_and_effort`
- `actor_model::jobs_without_a_role_table_start_as_before`
- `review_subagents::an_agent_that_names_codex_moves_alone_to_claude_and_keeps_where_its_provider_came_from`
- `runtime_review::a_passing_review_exits_the_live_session_and_lands_it`

JUnit の testcase は、10 本とも同じ run の Linux と macOS で同じ数（5,888〜5,949）である。4 本が落ちても残りは流れている。10 本の Linux の JUnit に `flakyFailure` は無い。

### 判定

**holds**。成功の条件に当てはめた根拠は次のとおり。

- 対象の run は 10 本（3 本以上）で、artifact はすべて読めた。
- (i) 対象の test は 10 本とも passed で、testcase に `failure`・`rerunFailure`・`flakyFailure` の子が無い（retry なし）。
- (ii) 10 本とも、Linux の failed か flaky は上の 4 本だけで、いずれも同じ run の macOS でも落ちた。Linux だけの failed か flaky は 0 本である。

task 3879 の前は、直前の 29 本で対象の test が 3 回（10%）落ちていた。10 本で 0 回という結果は、この割合なら偶然でも約 35% で起きるので、10 本だけでは直りの強い証拠ではない。下の補足の 34 本でも 0 回で、合わせて 44 本で 0 回である（10% のままなら約 1%）。

### 判定の範囲の後の run（補足）

判定の 10 本は 11-03 09:10 までで、この run の時点より 2 日半古い。そこで、判定には入れず、同じ方法で後の run を読んだ。読んだのは、41614815009（11-03 09:54）から 42090533878（11-05 19:05）までの、linux build / test が完了した 34 本である（skipped の 3 本は除く）。

対象の test は 34 本とも passed だった。上の 4 本は 34 本とも両方の OS で落ちた（41850367787 は macOS の artifact が無く、Linux だけを読んだ）。ほかに、Linux だけで落ちた test が 4 本、1 回ずつあった。

| run | head_sha | 時刻 | test | Linux の結果 | 同じ run の macOS | queue の対応 |
|---|---|---|---|---|---|---|
| 41748544812 | `2ee4e6de1` | 11-04 00:40 | `runtime_claim_class::the_spike_limit_defers_a_spike_and_gives_its_slot_to_an_implementation` | flaky（2 回目で通る） | passed | task 4186（ready） |
| 41771136549 | `67190bcc6` | 11-04 04:06 | `runtime_queue_hold_shared::cancel_affected_gives_up_the_held_runs_of_every_supervisor` | flaky（2 回目で通る） | passed | task 4191（ready） |
| 41949236669 | `f5bb9f280` | 11-05 01:32 | `runtime_handoff::review_verdicts::verdicts_kept_by_a_supervisor_that_stopped_are_applied_by_one_running_beside_it` | flaky（2 回目で通る） | passed | task 4265（ready） |
| 42090533878 | `a0f2ed9ce` | 11-05 19:05 | `runtime_headless::recovery_stops_a_headless_turns_orphan_outside_its_group_by_worktree` | failed（2 回とも `tests/it/runtime_headless.rs:1196` で panic） | passed | task 4288（draft） |

queue の状態は、この run の `dagq search` の時点の値である。`runtime_headless` の 1 本は、完了した run のうち最新の 1 本で出た。次の run 42096862747（`e4bdae32a`）の linux build / test は読んだ時点でまだ走っていて、続けて落ちるかは分からない。

34 本のうち 4 本（約 12%）で Linux だけの失敗が出たので、task 3925 の根拠の「ほかの test は全部 Linux で通った」は、今の main ではそのまま言えない。

### 後続への推奨（task 3879 の着地の後）

- description の条件どおり、3926・3927 は submit してよい。context には、この節と対象の run の id（41533298296〜41604256757 の 10 本）を書く。task 3879 の直しは対象の test に効いているので、これに関わる直しの task は先に要らない。
- submit の前に、planner が補足の 4 本を見て決める。推奨は次のとおり。
  - flaky の 3 本（task 4186・4191・4265）は、3926 の依存にしない。1 回ずつ、2 回目で通っていて、ready の task が既にある。
  - `runtime_headless` の 1 本（task 4288、draft）は、submit の前に後の run で続くかを読む。続くなら 4288 を submit して 3926 をそれに依存させる。1 回だけなら依存にしない。
  - 両 OS で落ちる 4 本（task 4076）は Linux に固有の問題ではないので、3926 の依存にしない。
- 3926 の job でも、`scripts/it-coverage-map.sh` の今の振る舞い（落ちた test も表に記録して表を作る）を保つ。Linux で 1 本も落ちない回を前提にすると、表が作れない回が出るからである。初回の数本では、Linux で failed・flaky の test を合わせて見る（上の「後続への推奨」の初回の記録と同じ）。
