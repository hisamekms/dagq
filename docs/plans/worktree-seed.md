---
id: plan-worktree-seed
type: plan
title: 新しいrunのworktreeに温まったtargetをAPFSのcloneで入れたときのbuildの短縮の見積もり（sccacheとの重なりを含めて）
status: completed
created: 2026-10-02
updated: 2026-10-02
owners:
  - hisamekms
tags:
  - performance
  - build
  - measurement
related:
  - plan-coverage-at-landing
  - plan-sccache-measurement
  - plan-nextest-post-test-stage
  - adr-0049
---

# 新しいrunのworktreeに温まったtargetをAPFSのcloneで入れたときのbuildの短縮の見積もり（sccacheとの重なりを含めて）

task 968の測定（goal 65）。runのworktreeの`target/`は空から始まる。依存crateは`[run.env]`の`RUSTC_WRAPPER = "sccache"`でrun間に共有している（[ADR-0049](../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定6）。この文書は、次の案の効果を見積もる。

- 案: provisioningで、mainの最新でbuildした温まった`target/`を、`cp -c`（APFSのclone。中身を複製しない）で新しいworktreeに入れる。

使い捨てのcloneで、3つの条件を交互に4周流して比べた。変更はせず、決定もしない。taskも登録しない。

## 要点

- **cloneしたtargetでも、dagq自身のcrate（`dagq`・`dagq-broker`・`dagq-broker-client`）は毎回build し直しになった。** 条件(c)でソースのmtimeを揃えても同じだった。cloneで省けたのは依存crateの段だけで、その段は空のtargetでもsccacheが当たって短い。
  - 依存crateの段（cargoの最初の出力行から`Compiling dagq v…`まで）の中央値は、(a)空のtargetで11〜19秒、(b)で3〜5秒、(c)で0〜0.6秒だった（2章）。
  - build全体の時間は、hostのloadの揺れ（1回で71〜453秒）に埋もれた。`cargo build`と`cargo test --no-run`では、条件ごとの差は見分けられなかった。
- **原因は、dagqのbuild scriptが絶対pathの`rerun-if-changed`を出し、worktreeごとに違うGitのファイルを見張っていること。** cargoのfingerprintのlogでは、build scriptが`RerunIfChangedOutputPathsChanged`でdirtyになった。記録された絶対pathがseedの場所を指していたためである。build scriptが走り直すと、出力が同じでもそれに依るcrateはcompileし直しになる。小さなcrateで両方を再現した（3章）。`env!("CARGO_MANIFEST_DIR")`はdirtyの原因にならなかった。
- **`cargo llvm-cov nextest`のbuildだけは、(c)が4周とも(a)より56〜84秒短かった。** (b)は2周で短く、2周で長かった。理由は確かめていない（3.2節）。(c)のmtimeの揃え方をそのまま本番に使うと、変わったファイルを見落とす危険がある（5章）。
- **1本あたりの短縮の見込みは小さい。** workerの最初のbuildで約10〜20秒、着地の検証のbuildで約10〜25秒。一方、cloneに2.3〜15.4秒（中央値6.4秒）かかる。runの作業の数十分に比べて1〜2%で、loadの揺れより小さい（4章）。
- **disk: cloneは実のblockを共有するが、`du`（block数）では1 worktreeあたり約5 GiB多く数えられる。** build後に新しく書かれた量は、(a)より3〜10%少ないだけだった。runtimeの容量の見積もり（`build_outputs_removed`の`bytes`）は`du`と同じ数え方なので、そのままでは閾値が膨らむ（4.3節）。
- **AGENTS.mdが挙げる、targetを共有しない理由(a)(b)には当たらない。** cloneはcopy-on-writeの独立した複製で、test binaryもprofrawも各worktreeのtargetに書かれる（6章）。
- **言語に依らない形（`dagq.toml`に複製するディレクトリを書く）は作れる。** ただし効果はbuild toolのcacheの鍵（mtime・絶対path・内容のhash）と、global cacheの有無で決まる。Rustのこのrepositoryでは、build scriptを直さない限り効果は小さい（7章）。

## 1. 条件

| 項目 | 値 |
| --- | --- |
| commit | `fb8170a8`（このrunのbase commit。そのときのmainの最新） |
| 作業場所 | 使い捨てのclone `<scratchpad>/seed`（`git clone --no-hardlinks`の後に`fb8170a8`をcheckout）。各試行のworktreeは`git -C seed worktree add --detach`で同じvolumeの`<scratchpad>/wt-…`に作り、試行の後に消した。dagqのqueueは作っていない。本番のqueue、このrepositoryのcheckout、固定バイナリには触れていない |
| 日時 | 2026-10-02 14:45〜16:50 JST（seedのbuildは14:38〜14:44） |
| host | 8コア（aarch64-apple-darwin）、16GB、APFS。本番のsupervisor（`parallel = 3`）の他のrunが走る負荷の下で測った。5秒ごとのload1は中央値19.2、最大54.7 |
| toolchain・ツール | rustc 1.98.1、cargo-llvm-cov 0.9.1、cargo-nextest 0.9.146、sccache 0.18.0（本番と共有のserver） |
| env | `CARGO_BUILD_JOBS=4`、`RUSTC_WRAPPER=sccache`、`SCCACHE_IGNORE_SERVER_IO_ERROR=1`、`RUST_TEST_THREADS=6`、`NEXTEST_TEST_THREADS=6`（mainの`dagq.toml`の`[run.env]`と同じ）。`DAGQ_*`は外した |

seed: `<scratchpad>/seed`で、tracked・untrackedのファイルとディレクトリのmtimeをcommitの時刻（2026-10-02 12:38:36）に揃えてから、下の3つのコマンドを順にbuildした。
- buildの時間: `cargo build`が207秒、`cargo test --no-run`が0.3秒（直前のbuildで足りた）、`cargo llvm-cov`が144秒。
- `target/`は5.0 GiB（14,113 file）。内訳は`debug/`が2.5 GiB、`llvm-cov-target/`が2.4 GiBで、どちらも`incremental/`が1.7 GiBを占める。

条件:

- **(a) 空のtarget + sccache**: worktreeを作ってそのままbuildする（今の本番と同じ）。
- **(b) cloneしたtarget + sccache**: worktreeを作った後に`cp -c -Rp seed/target <wt>/target`する。`-p`でmtimeを保つ。ソースはcheckoutの時刻なので、seedのtargetより新しい。
- **(c) cloneしたtarget + mtimeを揃えたソース**: (b)の前に、worktreeのファイルとディレクトリのmtimeをseedと同じcommitの時刻に揃える。

コマンド（buildの段だけを測るもの）:

- `build`: `cargo build --locked --all-targets`
- `test`: `cargo test --locked --no-run`
- `cov`: `cargo llvm-cov nextest --locked --workspace --no-report -E 'none()' --no-tests=pass`
  - testを1件も流さず、`target/llvm-cov-target`のbuildだけをさせる。

順番: 周ごとにコマンドを`build → test → cov`で回し、各コマンドの中で条件の順を周ごとにずらした（1周目a,b,c、2周目b,c,a、3周目c,a,b、4周目a,b,c）。時間とともに変わる本番のloadを条件に振り分けるためである（task 967の[coverage-at-landing](coverage-at-landing.md)で、時間が条件よりloadで決まったため）。試行は毎回新しいworktreeで行い、1試行ごとに1つのコマンドだけを流した。

測り方:
- 出力の各行にperlで時刻を付けた。
- build: コマンドの開始から、cargoの``Finished `…` profile``の行まで。
- 依存crateの段: コマンドの最初の出力行から、最初の`Compiling dagq v…`の行まで。コマンドの開始からだと0.1〜1.2秒長い。
- clone: `cp -c -Rp`の前後の時刻。
- 新しく書いた量: build後の`target/`の中で、build直前に置いた印より新しいfileのblock数の合計。hard linkは重ねて数えるので、条件の間の比較にだけ使う。
- `du`: build後の`du -sk target`。
- load1: 5秒ごとの`sysctl vm.loadavg`の1分値を、試行の間で平均と最大にした。

## 2. 結果

試行ごとの値（秒。dirtyのcrateは`Compiling`の行に出たworkspaceのcrate）:

| 周 | コマンド | 条件 | 開始 | build | 依存crateの段 | clone | worktree add | load1 平均／最大 | `Compiling`の数（workspaceのcrate） |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | build | a | 14:45 | 103.6 | 12.0 | — | 0.57 | 14.1／15.9 | 69（依存を含む全部） |
| 1 | build | b | 14:46 | 105.6 | 1.7 | 2.34 | 0.22 | 11.0／12.5 | 4（dagq・broker・client・protocol） |
| 1 | build | c | 14:48 | 116.7 | 0.0 | 3.64 | 0.38 | 11.9／13.5 | 3（dagq・broker・client） |
| 1 | test | a | 14:50 | 218.0 | 17.6 | — | 0.80 | 17.2／21.5 | 69 |
| 1 | test | b | 14:54 | 338.2 | 6.8 | 12.53 | 0.82 | 22.0／32.1 | 4 |
| 1 | test | c | 15:00 | 134.9 | 0.0 | 12.47 | 1.17 | 22.5／28.9 | 3 |
| 1 | cov | a | 15:03 | 193.0 | 8.6 | — | 0.16 | 15.9／21.0 | 69 |
| 1 | cov | b | 15:06 | 451.0 | 6.8 | 9.79 | 1.06 | 26.3／34.9 | 4 |
| 1 | cov | c | 15:14 | 109.3 | 0.6 | 4.56 | 0.90 | 17.9／23.2 | 3 |
| 2 | build | b | 15:16 | 370.5 | 6.5 | 10.25 | 1.08 | 24.4／31.8 | 4 |
| 2 | build | c | 15:23 | 452.9 | 0.0 | 13.09 | 1.04 | 32.5／50.1 | 3 |
| 2 | build | a | 15:30 | 286.0 | 37.7 | — | 0.66 | 35.7／46.6 | 69 |
| 2 | test | b | 15:35 | 146.8 | 5.0 | 5.28 | 0.34 | 20.5／26.8 | 4 |
| 2 | test | c | 15:38 | 360.7 | 0.0 | 15.41 | 2.66 | 31.2／41.4 | 3 |
| 2 | test | a | 15:44 | 410.3 | 30.9 | — | 0.74 | 33.9／54.7 | 69 |
| 2 | cov | b | 15:52 | 242.3 | 8.3 | 12.69 | 2.43 | 27.5／36.3 | 4 |
| 2 | cov | c | 15:56 | 95.0 | 0.5 | 5.01 | 0.37 | 14.4／17.2 | 3 |
| 2 | cov | a | 15:57 | 165.5 | 10.9 | — | 0.37 | 16.3／20.6 | 69 |
| 3 | build | c | 16:00 | 106.3 | 0.0 | 5.83 | 0.91 | 14.0／18.2 | 3 |
| 3 | build | a | 16:02 | 127.7 | 11.3 | — | 0.33 | 14.1／15.6 | 69 |
| 3 | build | b | 16:05 | 276.0 | 5.0 | 6.60 | 0.73 | 17.0／20.9 | 4 |
| 3 | test | c | 16:10 | 208.7 | 0.0 | 8.31 | 0.95 | 16.9／18.3 | 3 |
| 3 | test | a | 16:13 | 182.7 | 20.8 | — | 0.61 | 16.1／18.1 | 69 |
| 3 | test | b | 16:16 | 237.3 | 5.3 | 6.49 | 0.34 | 16.9／19.2 | 4 |
| 3 | cov | c | 16:21 | 185.0 | 1.2 | 10.03 | 2.35 | 17.8／21.2 | 3 |
| 3 | cov | a | 16:24 | 241.2 | 10.8 | — | 0.27 | 27.5／40.4 | 69 |
| 3 | cov | b | 16:28 | 81.5 | 2.8 | 6.68 | 0.45 | 19.6／28.6 | 4 |
| 4 | build | a | 16:29 | 125.5 | 6.8 | — | 0.26 | 12.6／15.3 | 69 |
| 4 | build | b | 16:32 | 71.3 | 1.7 | 3.03 | 0.25 | 10.8／12.3 | 4 |
| 4 | build | c | 16:33 | 112.6 | 0.0 | 2.80 | 0.38 | 12.9／14.9 | 3 |
| 4 | test | a | 16:35 | 107.5 | 6.7 | — | 0.24 | 11.3／13.7 | 69 |
| 4 | test | b | 16:37 | 135.4 | 3.2 | 6.41 | 0.61 | 12.5／13.8 | 4 |
| 4 | test | c | 16:39 | 202.8 | 0.0 | 5.83 | 0.67 | 17.1／19.6 | 3 |
| 4 | cov | a | 16:43 | 183.9 | 21.5 | — | 1.61 | 16.0／19.9 | 69 |
| 4 | cov | b | 16:46 | 89.6 | 1.8 | 2.51 | 0.30 | 11.1／12.3 | 4 |
| 4 | cov | c | 16:48 | 126.8 | 0.4 | 2.88 | 0.31 | 20.6／29.1 | 3 |

条件ごとの中央値（括弧は範囲）:

| コマンド | 条件 | build（秒） | 依存crateの段（秒） | clone（秒） | disk: 新しく書いた量（GiB） | disk: build後の`du`（GiB） | load1の平均 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| build | (a) 空 | 126.6（103.6〜286.0） | 11.7（6.8〜37.7） | — | 2.97（2.96〜2.97） | 2.5（2.5〜2.5） | 14.1（12.6〜35.7） |
| build | (b) clone | 190.8（71.3〜370.5） | 3.4（1.7〜6.5） | 4.8（2.3〜10.3） | 2.86（2.81〜2.91） | 7.4（7.3〜7.4） | 14.0（10.8〜24.4） |
| build | (c) clone + mtime | 114.6（106.3〜452.9） | 0.0 | 4.7（2.8〜13.1） | 2.74（2.73〜2.81） | 7.3（7.3〜7.3） | 13.5（11.9〜32.5） |
| test | (a) 空 | 200.3（107.5〜410.3） | 19.2（6.7〜30.9） | — | 2.83（2.78〜2.86） | 2.3（2.3〜2.4） | 16.7（11.3〜33.9） |
| test | (b) clone | 192.1（135.4〜338.2） | 5.2（3.2〜6.8） | 6.5（5.3〜12.5） | 2.68（2.64〜2.72） | 7.4（7.4〜7.4） | 18.7（12.5〜22.0） |
| test | (c) clone + mtime | 205.7（134.9〜360.7） | 0.0 | 10.4（5.8〜15.4） | 2.59（2.54〜2.64） | 7.3（7.3〜7.4） | 19.8（16.9〜31.2） |
| cov | (a) 空 | 188.5（165.5〜241.2） | 10.9（8.6〜21.5） | — | 2.94（2.91〜2.97） | 2.4（2.4〜2.5） | 16.2（15.9〜27.5） |
| cov | (b) clone | 166.0（81.5〜451.0） | 4.8（1.8〜8.3） | 8.2（2.5〜12.7） | 2.76（2.75〜2.87） | 7.4（7.4〜7.5） | 22.9（11.1〜27.5） |
| cov | (c) clone + mtime | **118.0（95.0〜185.0）** | 0.6（0.4〜1.2） | 4.8（2.9〜10.0） | 2.68（2.67〜2.70） | 7.4（7.4〜7.4） | 17.9（14.4〜20.6） |

diskの欄:
- 新しく書いた量は、build直前に置いた印より新しいfileのblock数の合計で、hard linkを重ねて数える（1章）。cloneしたtargetでは、これがそのworktreeだけが持つ実のblockにおおむね当たり、seedと共有するblockは含まない。
- `du`はcloneで共有しているblockもfileごとに数えるので、(b)・(c)ではseedの5.0 GiBの分だけ大きく出る。
- seedそのもの（5.0 GiB）はqueueに1つ要る。cloneの前後の`df`の差は、並行する本番のrunの書き込みで−0.8〜+0.8 GiBに揺れ、cloneそのものの使用量は分けられなかった（4.3節）。

読み方:

- **cloneで消えたのは依存crateの段だけだった。** (a)では`Compiling`の行が69（依存crate 65とworkspaceのcrate 4）。(b)はworkspaceの4つ、(c)は`dagq-broker-protocol`を除く3つだった。
- **依存crateの段は、空のtargetでも中央値11〜19秒と短い。** 依存crateはsccacheに当たるためである（[sccache-measurement](sccache-measurement.md)）。buildの残り（dagqのcrateのcompileと、test binaryのlink）は、どの条件でも同じだけ必要になった。
- **`build`と`test`では、条件の間の差がloadの揺れに埋もれた。** 同じ条件の中でも時間は2〜5倍に広がった。各周で最短だった条件は、`build`ではa・a・c・b、`test`ではc・b・a・aで、決まった順は無かった。
- **`cov`だけは(c)が4周とも(a)より短かった。** 差は84・71・56・57秒で、(a)の23〜43%にあたる。load1の平均は(c)が14.4〜20.6、(a)が15.9〜27.5で、3周目を除けば近い。(b)は4周中2周で(a)より短く、残り2周で長かった。

## 3. dagqのcrateがbuildし直しになる理由

`CARGO_LOG=cargo::core::compiler::fingerprint=info`で(c)の`build`を1回流し、cargoが出すdirtyの理由を見た。
- `dagq`・`dagq-broker`・`dagq-broker-client`のbuild scriptは、どれも`RerunIfChangedOutputPathsChanged`でdirtyになった。
- cloneしたtargetのfingerprintには、build scriptの`rerun-if-changed`がseedの根からの相対path（`src`・`.git/HEAD`・`.git/index`など。logの`old`）で入っていた。一方、cloneしたbuild scriptの出力から読み直したpathは、seedの絶対path（`<scratchpad>/seed/src`、`<scratchpad>/seed/crates/dagq-broker/../../.git/HEAD`など。logの`new`）で、worktreeの根からは相対pathに直せず、食い違った。
- これらに依るlib・bin・test targetは、すべて`UnitDependencyInfoChanged`でdirtyになった。
- `dagq-broker-protocol`にはbuild scriptが無く、(c)ではfreshだった。

絶対pathを出しているのは、次の2つである。
- `crates/dagq-broker-protocol/src/build_id.rs`の`emit`（build識別子。`root.join(path)`で、`src`・`crates`・`.git/HEAD`・`.git/index`・`.git/packed-refs`などを見張る）。
- `build.rs`の`embed_broker_material`（`manifest_dir.join("rust-toolchain.toml")`と、brokerのimageの材料）。

もう1つ、worktreeでは`git rev-parse --git-path index`が`<common dir>/worktrees/<name>/index`になる。このfileはworktreeごとに違い、checkoutの時刻を持つ。build scriptが相対pathを出すように直しても、Gitの状態を見張る限り、新しいworktreeではbuild scriptが走り直す見込みが高い（確かめてはいない）。

小さなcrateで確かめたこと（`<scratchpad>/tiny`、sccacheなし）:

- `env!("CARGO_MANIFEST_DIR")`を使うlibを`cp -c -Rp`で別の場所に移し、mtimeを揃えたまま`cargo build`した。dirtyにならずfreshだった。dagqの`src/`の4つのfileが使う`env!("CARGO_MANIFEST_DIR")`は、原因ではない。
- build scriptが`rerun-if-changed={CARGO_MANIFEST_DIR}/stamp`（絶対path）を出すcrateを同じように移した。`RerunIfChangedOutputPathsChanged { old: ["stamp"], new: ["<元の場所>/stamp"] }`でdirtyになり、libもcompileし直しになった。
- build scriptが`rerun-if-changed=stamp`だけを出し、`rustc-env`に同じ値を出すcrateで、`stamp`だけをtouchした。build scriptが走り直し、出力が同じでもlibはcompileし直しになった。

### 3.1 mtimeを揃えない(b)の振る舞い

(b)では、checkoutの時刻を持つソースが、seedのtargetより新しくなる。build scriptに加えて`dagq-broker-protocol`もdirtyになり、`Compiling`の行は4つになった。依存crate（registryのsource）はmtimeが変わらないのでfreshのままで、(b)の依存crateの段が短いのはこのためである。

### 3.2 `cov`で(c)が短かった理由（確かめていない）

`cov`のbuildで(c)だけが4周とも短かった理由は、確かめていない。考えられるのは次のことである。
- (c)では`dagq-broker-protocol`がfreshのまま残る。そのため、それに依るdagqのcrateのincremental cache（cloneした`incremental/`）が使い回された可能性がある。
- (b)ではそのcrateが作り直されるので、依るcrateのincrementalが当たりにくい。

`build`と`test`で同じ差が見えないことは、これだけでは説明できない。stableのrustcでincrementalの再利用量を出す方法が無いので、この測定では分けられなかった。

## 4. 1本あたりの短縮の見込み

### 4.1 本番の着地の検証のbuild

本番のrun dirに残っている、llvm-covを流した着地の検証のlogを読んだ。85件で、2026-09-30〜10-02の分である（`integrate-*-verify-*.log`の``Finished `test` profile … in``の値）。

| `target/llvm-cov-target`の状態 | 件数 | build 中央値 | 四分位（25%〜75%） | 範囲 |
| --- | --- | --- | --- | --- |
| 空（`Compiling` 69。runの最初の着地の試行） | 70 | 94秒 | 65〜124秒 | 51〜300秒 |
| 温まっている（`Compiling` 3〜4（14件が4、1件が3）。同じworktreeで2回目以降の試行） | 15 | 72秒 | 52〜104秒 | 44〜229秒 |

- 温まった`llvm-cov-target`でも、workspaceの4つのcrateは毎回buildし直しになっている。本番の差（中央値で約22秒）は、この測定の依存crateの段（`cov`の(a)で中央値10.9秒、最大21.5秒）とおおむね合う。
- task 563・564の35〜55秒は、brokerのcrate（ADR-t827-1）が入る前の値である。いまの着地の検証のbuildは、それより長い。
- 着地の検証は、runのworktreeでrebaseの後に流れる（[`integrate`](../design/supervisor-lifecycle/integrate.md#integrate)の5）。workerは`cargo llvm-cov`を流さないので、最初の試行の`llvm-cov-target`は今は空である。seedの`llvm-cov-target`をprovisioningでcloneしておけば、この依存crateの段の分（**約10〜25秒**）が縮む見込みである。
- 本番では、着地はrebaseで変わったソースと、runの変更の上で行われる。(c)のように揃った状態にはならないので、3.2節の56〜84秒の差をそのまま見込みにはしない。

### 4.2 workerのbuild

- workerのbuildは、1本あたり中央値1.3〜3分である（taskのdescription。task 563・564）。
- cloneで縮むのは、worktreeで最初に流すbuild（`cargo build`・`cargo clippy`・`cargo test`）の依存crateの段だけである。この測定では`build`で中央値11.7秒、`test`で19.2秒だった。
- 2回目以降のbuildは、今も同じworktreeのtargetを使うので変わらない。
- `cargo clippy`はcheckのmetadataを別に作るので、seedにclippyの成果物を入れなければ縮まない（この測定では測っていない）。
- 見込み: workerで**約10〜20秒**。

### 4.3 cloneの時間とdisk

- cloneの時間: 5.0 GiB・14,113 fileの`cp -c -Rp`で、2.3〜15.4秒（中央値6.4秒）。loadが高いほど長かった。`git worktree add`は0.16〜2.7秒（中央値0.61秒）。
  - provisioningの経路で毎回この時間がかかるので、依存crateの段で縮む10〜20秒の半分ほどを打ち消す。
- 実のdisk:
  - cloneは、書き換えるまでblockをseedと共有する。
  - build後に新しく書かれた量（hard linkを重ねて数えた値）は、(a)が2.78〜2.97 GiB、(b)が2.64〜2.91 GiB、(c)が2.54〜2.81 GiBだった。cloneで減るのは3〜10%ほどだけである。dagqのcrateのbuild成果物（test binaryと`incremental/`）がtargetの大半を占め、それが毎回書き直されるためである。
  - これとは別に、seed自身の5.0 GiBがqueueに1つ要る。
  - cloneの前後の`df`の差は、並行する本番のrunの書き込みで−0.8〜+0.8 GiBに揺れた。cloneそのものの使用量は、この測り方では分けられなかった。
- `du`での数え方:
  - build後の`du -sk target`は、(a)が2.3〜2.5 GiB、(b)・(c)が7.3〜7.5 GiBだった。`du`はcloneで共有しているblockもfileごとに数える。
  - [Run worktrees](../design/supervisor-lifecycle/run-worktrees.md)の`build_outputs_removed`の`bytes`（blocks × 512）と、それを使う[空き容量](../design/supervisor-lifecycle/disk-space.md)の閾値は、同じ数え方をする。そのままでは、1 runあたり約5 GiBを多く見込む。cloneしたtargetを消しても、空くのは共有していないblockだけである。

### 4.4 まとめ

| 対象 | 今（本番） | cloneで縮む見込み | cloneのコスト |
| --- | --- | --- | --- |
| workerの最初のbuild | 1.3〜3分 | 約10〜20秒 | provisioningでclone 2〜15秒 |
| 着地の検証のbuild（最初の試行） | 中央値94秒 | 約10〜25秒（`llvm-cov-target`もcloneする場合） | 同上（同じcloneに含む） |
| 1 runあたり | — | 約20〜45秒から、cloneの時間（約6秒）を引いた分 | disk: seed 5.0 GiB（queueに1つ）と、runtimeの容量の見積もりの直し |

1 runの作業と着地は数十分かかり、この測定でもbuildの時間はloadで2〜4倍揺れた。この短縮は1〜2%で、着地の件数の変化として測れる大きさではない。

効果を大きくするには、dagqのcrate自身をfreshにするか、incrementalに当てる必要がある。そのための前提は次の2つで、どちらもこの測定の範囲の外である。
- build scriptを直す（絶対pathをやめる。worktreeごとに違うGitのファイルを見張らない）。
- 変わっていないファイルにだけseedのmtimeを入れる。

## 5. mtimeを揃えるときの注意

(c)は、seedと同じcommitで、全ファイルのmtimeをcommitの時刻に揃えた。本番では、seedがbuildしたcommitと、runのbase commitが違いうる。全ファイルを一律に古い時刻へ揃えると、次の場合にcargoが変更を見落とす（変わったファイルがseedのdep-infoより古く見え、古いbuild成果物がfreshと判定される）。
- seedのcommitからbase commitまでの間に変わったファイルがある。
- かつ、揃えた時刻がseedのbuildの時刻より前になる。

安全な形は、次のとおりである。
- seedのcommitとbase commitで内容が同じファイルにだけ、seedのcheckoutのmtimeを入れる。
- 変わったファイルは、checkoutの時刻のまま残す。

cloneをmtimeを保たずに行う（`-p`なし）と、targetのファイルがcloneの時刻、つまりソースより新しくなる。これも同じ見落としを起こすので、mtimeを保つ必要がある。

## 6. targetを共有しない理由に当たらないことの確認

AGENTS.mdは、runごとの`CARGO_TARGET_DIR`を共有しない理由として次の2つを挙げる（ADR-0049）。

- (a) 並行する別のrunのbuildが`target/debug/dagq`を上書きし、`CARGO_BIN_EXE_dagq`をexecするtestがそれを実行しうる。
- (b) 同時の`cargo llvm-cov`が、共有の`llvm-cov-target`のprofrawを消し合い、混ぜ合う。

cloneは共有ではない。
- APFSのcloneはcopy-on-writeの独立したfileである。worktreeのbuildが書き換えたfileは、そのworktreeだけの新しいblockになり、seedも他のrunのcloneも変わらない。
- (a)について: (b)・(c)でも、dagqのcrateとtest binaryは毎回そのworktreeの`target/`でlinkし直された（3章）。`CARGO_BIN_EXE_dagq`は、build時にそのworktreeのtargetの絶対pathとして埋め込まれる。
- (b)について: profrawは、そのworktreeの`target/llvm-cov-target`に書かれる。

残る注意は次の3つである。
- seedを作り直している最中にcloneすると、途中の状態を写す。別のディレクトリでbuildしてからrenameで差し替えるか、lockが要る。
- seedに`cargo llvm-cov`のprofrawを残さない（testを流さずにbuildする）。
- seedの`incremental/`には、seedのcommitのcompileの状態が入る。rustcはincrementalのcacheを自分で検証するので、正しさには影響しない見込みである。

## 7. 言語に依らない形の見立て

- **形**: `dagq.toml`に、provisioningで複製するディレクトリ（repository rootからの相対path）と、seedを作るコマンドを書く。例: `[worktree.seed] paths = ["target"]`と`build = ["cargo build --locked --all-targets", …]`。
  - runtimeは、main checkoutの横の専用のcheckoutでseedをbuildする。`--auto-update`の専用のcheckoutと同じ置き方である。
  - 新しいworktreeには、cloneで入れる。
  - Rustに依る知識は`dagq.toml`の値だけで、runtimeはディレクトリを複製するだけになる。
- **複製の手段は、filesystemに依る。**
  - APFS（macOS）: `clonefile`か`cp -c`。
  - Linuxのbtrfs・XFS: reflink（`cp --reflink=auto`）。
  - ext4やcontainerのoverlayにはcloneが無く、全部の複製になる。5 GiBなら時間もdiskも重く、効果より大きい。adapterはcloneできないときに複製をやめる（何もしない）必要がある。
  - seedとworktreeが同じvolumeにあることも条件になる。containerでは、seedをread-onlyのlayerかvolumeに置く形になり、1 worker 1 containerと常駐containerで置き方が変わる（goal 58・59）。
- **効果はbuild toolのcacheの鍵で決まる。**
  - mtimeで判定するtool（cargo・make）: checkoutの時刻で作り直しになる。5章の揃え方が要る。
  - 絶対pathを持つ成果物（cargoのbuild scriptの`rerun-if-changed`、Pythonのvenvのshebangとscript、一部のnode_modulesのbin）: 別の場所では無効になるか、壊れる。
  - 内容のhashで判定し、global cacheを持つtool（Goのbuild cache、Bazel、Gradleのbuild cache、sccache）: worktreeのディレクトリを複製しなくても、すでにrun間で共有できている。複製の上乗せは小さい。
  - このrepositoryでは、依存crateはsccacheが担い、worktreeごとに残るのはdagqのcrateだけである。そのdagqのcrateがbuild scriptのせいで当たらないので、効果は依存crateの段に限られた。
- **runtimeの他の部分への影響**:
  - 4.3節の容量の数え方（cloneで共有しているblockを除く数え方）。
  - [Run worktrees](../design/supervisor-lifecycle/run-worktrees.md)の掃除（cloneしたtargetを消しても、共有分は空かない）。
  - seedの作り直しの契機（mainへの着地ごとか、時間ごと）と、そのbuildのload（seedのbuildそのものが、着地ごとに1本の重いbuildを足す）。

## 8. この測定で分からないこと

- 3.2節の、`cov`で(c)が短かった理由。incrementalの再利用かどうか。
- build scriptを直した場合と、変わっていないファイルにだけseedのmtimeを入れた場合に、dagqのcrateのbuildがどれだけ縮むか。この場合でも、runの変更とrebaseで変わったcrateはcompileし直しになる。
- seedを最新に保つためのbuildが、hostのloadに足す量。
- `cargo clippy`の成果物のseed。
