---
id: plan-build-script-rerun-paths
type: plan
title: build scriptのrerun-if-changedをworktreeに依らない形にしたときの、cloneしたtargetでのdagqのcrateのbuildの短縮の測定
status: completed
created: 2026-10-05
owners:
  - hisamekms
tags:
  - performance
  - build
  - measurement
related:
  - plan-worktree-seed
  - plan-landing-lane-cpu-share
  - adr-0049
---

# build scriptのrerun-if-changedをworktreeに依らない形にしたときの、cloneしたtargetでのdagqのcrateのbuildの短縮の測定

task 1359の測定（goal 36）。[worktree-seed](worktree-seed.md)（task 968）では、温まった`target/`をAPFSのcloneで新しいworktreeに入れても、`dagq`・`dagq-broker`・`dagq-broker-client`は毎回buildし直しになった。原因はbuild scriptが絶対pathの`rerun-if-changed`を出し、worktreeごとに違うGitのファイルを見張ることだった（その3章）。この文書は、使い捨てのcloneの中でだけbuild scriptを直して、dagqのcrateがfreshかincrementalに当たるかを測る。変更はせず、決定もしない。taskも登録しない。

## 要点

- **build scriptを直しても、本番に近い条件ではdagqのcrateはbuildし直しになった。** 直したbuild script（(e)）では、build scriptがdirtyになる理由が「絶対pathの食い違い」から「見張っている`src`の中のファイルが新しい」に変わっただけだった。runの変更やrebaseで`src/`のファイルが1つでも変われば、3つのbuild scriptはどれも走り直す。3つとも`src`・`crates`などの同じ組を見張るためである（`build_id::SOURCES`）。build scriptが走り直すと、それに依るcrateはcompileし直しになる（2章・3章）。
- **incrementalには当たらない。** rustcは、作業ディレクトリ（`working_dir`）が違うとincrementalのcacheを丸ごと捨てる（`completely ignoring cache because of differing commandline arguments`）。cargoはworkspaceのcrateをworkspaceの根で compileするので、worktreeが違えば`working_dir`も違う。stableのrustcではこれを揃える手段が無い（4.1節）。
- **freshになるのは、seedと同じcommitで、変更が無いときだけ。** 直したbuild scriptで、seedと同じcommitのworktreeは`build`・`test`・`cov`とも0.2〜0.8秒で終わった（`Compiling`が0）。ただし、その後に`src/`を1つ変えた最初のbuildは、incrementalが使えないのでdagqのcrateを丸ごとcompileする。着地の検証はrebaseとrunの変更の上で流れるので、この場合に当たらない（4.2節）。
- **(d)と(e)の差は、hostのloadの揺れに埋もれた。** 縮んだのは、(a)に対する依存crateの段（`build`で中央値15.5秒、`test`で10.5秒、`cov`で13.4秒）だけで、これは(d)でも(e)でも同じだった。dagqのlibのcompileは、どの条件でも40〜190秒で、loadで決まった（2章）。
- **worktree-seed.mdの3.2節の、`cov`で(c)が短かった理由は、incrementalの再利用ではない。** 低いloadで(c)と同じ形（seedと同じcommit、直していないbuild script、mtimeを揃えたもの）を2回流すと、隣り合う(a)より10〜14秒長く、短くはならなかった。task 968の差は再現せず、理由は確かめきれなかった（4.3節）。
- **1本あたりの短縮の見込みは、worktree-seed.mdの4章より小さい。** workerの最初のbuildで約10〜20秒、着地の検証のbuildで約15〜20秒。cloneに約4秒かかる。seedを最新に保つbuildは、空から作り直せば着地ごとに約5分の重いbuildを足す（同じ場所で作り直したときの量は測っていない。5章）。
- **試作したbuild scriptの直し方は、build識別子の正しさを弱める。** 相対pathにすることは正しさを変えないが、worktreeごとのGitのファイル（`HEAD`・`index`・branchのref）を見張らないと、ソースを変えないcommitの後にbuild識別子が古いcommitを名乗る（6章）。

## 1. 条件

| 項目 | 値 |
| --- | --- |
| 作業場所 | runのdirの下の使い捨ての`<scratch>`。`git clone --no-hardlinks`で`<scratch>/repo`を作り、seedとbranchをその中にだけ作った。各試行のworktreeは`git -C repo worktree add --detach <scratch>/wt-…`で作り、試行の後に消した。dagqのqueueは作っていない。本番のqueue、このrepositoryのcheckout、固定バイナリには触れていない |
| 日時 | 2026-10-05 06:59〜08:37 JST（seedのbuildは06:47〜06:58） |
| host | 8コア（aarch64-apple-darwin）、16GB、APFS。本番のsupervisor（`parallel = 3`）の他のrunが走る負荷の下で測った |
| toolchain・ツール | rustc 1.98.1、cargo-llvm-cov 0.9.1、cargo-nextest 0.9.146、sccache 0.18.0（本番と共有のserver） |
| env | `CARGO_BUILD_JOBS=4`、`RUSTC_WRAPPER=sccache`、`SCCACHE_IGNORE_SERVER_IO_ERROR=1`、`RUST_TEST_THREADS=6`、`NEXTEST_TEST_THREADS=6`（mainの`dagq.toml`の`[run.env]`と同じ）。`DAGQ_*`は外した。全試行で`CARGO_LOG=cargo::core::compiler::fingerprint=info` |

commitとbranch（本番に近づけるため、seedとworktreeのcommitを違え、worktreeにrunの変更を入れた。worktree-seed.mdの5章の安全な形）:

- seedのcommit: `1a43a778`（このrunのbase `445f23e6`の2つ前のmain）。seedの後にmainが2回着地した状態にあたる。
- worktreeのcommit: `445f23e6`に、runの変更にあたる小さな変更（`src/lib.rs`に6行の関数）を1 commit足したもの。seedとの差は`src/`の4ファイル（`src/domain/stats.rs`・`src/domain/stats/asks.rs`・`src/domain/stats/landing_waits.rs`（新規）・`src/lib.rs`）と`tests/it/runtime_recheck.rs`で、`crates/`・`build.rs`・`Cargo.*`・`migrations/`は同じ。
- 直したbuild script（6章）は、seedとworktreeの両方の上に1 commitとして載せた（`seed-f`と`wt-f`）。直していないもの（`seed-u`と`wt-u`）とは、そのcommitの有無だけが違う。
- seedは2つ: 直していないもの（cloneの本体のcheckout）と、直したもの（同じcloneの`git worktree`）。それぞれで`build`・`test`・`cov`を順に流した。どちらも`target/`は5.5 GiB。`cov`がbuild scriptの実行で残したprofrawは消した。

条件:

- **(a) 空のtarget + sccache**: `wt-u`のworktreeでそのままbuildする（今の本番と同じ）。
- **(d) cloneしたtarget + 直していないbuild script + mtimeの揃え**: `wt-u`のworktreeに、直していないseedの`target/`を`cp -c -Rp`で入れ、変わっていないファイルにだけseedのmtimeを入れる。
- **(e) cloneしたtarget + 直したbuild script + mtimeの揃え**: `wt-f`のworktreeに、直したseedの`target/`を同じように入れる。

mtimeの揃え方: seedとworktreeのcommitを`git ls-tree -r -t`で比べ、blobのhashが同じファイル（1,048）にだけseedのcheckoutのmtimeを入れた。blobが違うファイル（9。上の`src/`の4つと`tests/`の1つ、`docs/`のファイル）はcheckoutの時刻のまま残した。ディレクトリは、直下の名前の組が同じもの（88）にだけseedのmtimeを入れた。

コマンド（worktree-seed.mdと同じ。`build`と`test`には`--timings`を足し、unitごとの時間を取った）:

- `build`: `cargo build --locked --all-targets --timings`
- `test`: `cargo test --locked --no-run --timings`
- `cov`: `cargo llvm-cov nextest --locked --workspace --no-report -E 'none()' --no-tests=pass`

順番: 3周。周ごとにコマンドを`build → test → cov`で回し、各コマンドの中で条件の順を周ごとにずらした（1周目a,d,e、2周目d,e,a、3周目e,a,d）。試行は毎回新しいworktreeで行い、1試行で1つのコマンドだけを流した。

測り方: worktree-seed.mdの1章と同じ（buildはコマンドの開始から``Finished``の行まで、依存crateの段は最初の出力行から最初の`Compiling dagq`の行まで、cloneは`cp -c -Rp`の前後、load1は5秒ごとの`sysctl vm.loadavg`の1分値の試行の間の平均と最大）。diskの使用量は測っていない。

diskの扱い: 同時に残すのはseed 2つ（11 GiB）と試行中のworktree 1つだけにした。試行の前に空きが22 GiBより少なければ待った（本番の`disk_space`の控えの閾値は18.5 GB前後。[landing-lane-cpu-share](landing-lane-cpu-share.md)の3.4節と5章）。本番のrunの書き込みで空きが20〜21 GiBに下がり、2回（08:00前後と08:15前後）待った。

## 2. 結果

試行ごとの値（秒。load1は平均／最大）:

| 周 | コマンド | 条件 | 開始 | build | 依存crateの段 | clone | load1 | `Compiling`の数（workspaceのcrate） |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | build | a | 06:59 | 201.4 | 15.6 | — | 22.7／29.5 | 69（依存を含む全部） |
| 1 | build | d | 07:03 | 200.4 | 0.1 | 4.6 | 18.6／22.9 | 3（dagq・broker・client） |
| 1 | build | e | 07:10 | 170.7 | 0.1 | 2.9 | 28.6／94.3 | 3 |
| 1 | test | a | 07:13 | 141.7 | 9.8 | — | 45.2／79.6 | 69 |
| 1 | test | d | 07:15 | 110.6 | 0.1 | 5.3 | 21.7／27.2 | 3 |
| 1 | test | e | 07:17 | 110.8 | 0.1 | 4.5 | 16.6／22.3 | 3 |
| 1 | cov | a | 07:19 | 93.3 | 13.4 | — | 10.9／13.5 | 69 |
| 1 | cov | d | 07:21 | 68.3 | 0.4 | 2.6 | 7.7／8.4 | 3 |
| 1 | cov | e | 07:22 | 68.9 | 0.4 | 3.1 | 7.7／9.0 | 3 |
| 2 | build | d | 07:24 | 75.5 | 0.1 | 2.4 | 6.4／8.1 | 3 |
| 2 | build | e | 07:25 | 94.1 | 0.1 | 3.6 | 10.3／13.3 | 3 |
| 2 | build | a | 07:27 | 171.4 | 7.0 | — | 14.6／17.1 | 69 |
| 2 | test | d | 07:30 | 205.7 | 0.2 | 7.9 | 15.0／16.8 | 3 |
| 2 | test | e | 07:34 | 120.8 | 0.1 | 7.0 | 13.7／19.2 | 3 |
| 2 | test | a | 07:36 | 276.4 | 10.5 | — | 71.0／118.7 | 69 |
| 2 | cov | d | 07:41 | 172.9 | 1.3 | 5.4 | 42.4／71.2 | 3 |
| 2 | cov | e | 07:44 | 242.8 | 0.7 | 4.7 | 51.4／113.9 | 3 |
| 2 | cov | a | 07:48 | 200.4 | 15.1 | — | 59.0／117.8 | 69 |
| 3 | build | e | 07:52 | 98.4 | 0.1 | 3.6 | 17.8／21.7 | 3 |
| 3 | build | a | 07:53 | 134.3 | 15.5 | — | 15.0／18.0 | 69 |
| 3 | build | d | 07:56 | 142.7 | 0.1 | 4.0 | 15.5／17.9 | 3 |
| 3 | test | e | 08:06 | 275.1 | 0.1 | 3.9 | 62.0／141.0 | 3 |
| 3 | test | a | 08:11 | 425.4 | 24.6 | — | 98.7／157.9 | 69 |
| 3 | test | d | 08:25 | 93.0 | 0.1 | 3.5 | 10.5／11.3 | 3 |
| 3 | cov | e | 08:27 | 76.6 | 0.4 | 2.8 | 8.4／9.8 | 3 |
| 3 | cov | a | 08:28 | 105.0 | 7.2 | — | 9.5／12.2 | 69 |
| 3 | cov | d | 08:30 | 99.4 | 0.4 | 4.0 | 11.7／12.9 | 3 |

条件ごとの中央値（括弧は範囲。3周）:

| コマンド | 条件 | build（秒） | 依存crateの段（秒） | buildから依存crateの段を引いた残り（秒） | clone（秒） | load1の平均 |
| --- | --- | --- | --- | --- | --- | --- |
| build | (a) 空 | 171.4（134.3〜201.4） | 15.5（7.0〜15.6） | 164.4（118.8〜185.8） | — | 15.0（14.6〜22.7） |
| build | (d) clone + 直していない | 142.7（75.5〜200.4） | 0.1（0.1〜0.1） | 142.6（75.4〜200.3） | 4.0（2.4〜4.6） | 15.5（6.4〜18.6） |
| build | (e) clone + 直した | 98.4（94.1〜170.7） | 0.1（0.1〜0.1） | 98.3（94.0〜170.6） | 3.6（2.9〜3.6） | 17.8（10.3〜28.6） |
| test | (a) 空 | 276.4（141.7〜425.4） | 10.5（9.8〜24.6） | 265.9（131.9〜400.8） | — | 71.0（45.2〜98.7） |
| test | (d) clone + 直していない | 110.6（93.0〜205.7） | 0.1（0.1〜0.2） | 110.5（92.9〜205.5） | 5.3（3.5〜7.9） | 15.0（10.5〜21.7） |
| test | (e) clone + 直した | 120.8（110.8〜275.1） | 0.1（0.1〜0.1） | 120.7（110.7〜275.0） | 4.5（3.9〜7.0） | 16.6（13.7〜62.0） |
| cov | (a) 空 | 105.0（93.3〜200.4） | 13.4（7.2〜15.1） | 97.8（79.9〜185.3） | — | 10.9（9.5〜59.0） |
| cov | (d) clone + 直していない | 99.4（68.3〜172.9） | 0.4（0.4〜1.3） | 99.0（67.9〜171.6） | 4.0（2.6〜5.4） | 11.7（7.7〜42.4） |
| cov | (e) clone + 直した | 76.6（68.9〜242.8） | 0.4（0.4〜0.7） | 76.2（68.5〜242.1） | 3.1（2.8〜4.7） | 8.4（7.7〜51.4） |

cloneの時間は、全18回で中央値3.9秒（2.4〜7.9秒）だった。

`--timings`で取ったunitごとの時間（`build`と`test`の18回。秒）:

| 条件 | dagqのlibの開始 | dagqのlibのcompile | `dagq "lib" (test)`のcompile |
| --- | --- | --- | --- |
| (a) | 12〜49 | 62〜188 | 99〜330 |
| (d) | 1〜2 | 40〜121 | 64〜175 |
| (e) | 1 | 54〜153 | 84〜247 |

読み方:

- **(d)と(e)は、どちらもworkspaceの3つのcrateがbuildし直しになった。** `Compiling`の行は(d)・(e)とも`dagq`・`dagq-broker`・`dagq-broker-client`の3つで、`dagq-broker-protocol`はfreshだった。(a)は依存crateを含む69だった。
- **cloneで縮んだのは、dagqのlibが始まるまでの時間だけだった。** (a)ではdagqのlibは12〜49秒後に始まり（依存crateと`dagq-broker-protocol`、build scriptのcompileと実行を待つ）、(d)・(e)では1〜2秒後に始まった。dagqのlibのcompileそのものは、どの条件でも40〜190秒で、条件より同時のloadで決まった。
- **(d)と(e)の差は見分けられない。** 中央値では`build`と`cov`で(e)が短く、`test`で(d)が短かった。同じ条件の中で時間は2〜3倍に広がった。build scriptを直してもdagqのcrateがcompileし直しになる（3章）ので、差が出る理由も無い。
- **`test`の(a)は、2周目と3周目が本番の重い時間（load1の平均71と99）に当たった。** `test`の中央値の(a)と(d)・(e)の差（150秒以上）は、この揺れによるもので、条件の差ではない。

## 3. buildし直しになったcrateとdirtyの理由

全試行の`CARGO_LOG=cargo::core::compiler::fingerprint=info`の出力から、dirtyの理由を拾った（3周とも同じだった）。

| 条件 | 3つのbuild scriptの実行（`build-script-build`）のdirtyの理由 | dagqのlib | それ以外のlib・bin・test |
| --- | --- | --- | --- |
| (a) | 空のtargetなので、全unitが新しい | — | — |
| (d) | `RerunIfChangedOutputPathsChanged`。`old`はseedのbuildで記録した相対path（`src`・`.git/HEAD`・`.git/index`・`.git/packed-refs`・`.git/refs/heads/seed-u`など。crateは`../../src`など）、`new`はcloneしたbuild scriptの出力から読み直したseedの絶対path（`<scratch>/repo/crates/dagq-broker/../../src`など） | 上に加えて、`stale: changed "<wt>/src/domain/stats.rs"`（変わったソース） | `UnitDependencyInfoChanged` |
| (e) | `FsStatusOutdated(StaleItem(ChangedFile { stale: "<wt>/src" … }))`。見張っている`src`のディレクトリの中に、build scriptの出力より新しいファイル（runの変更とrebaseで変わったファイル）がある。3つのbuild scriptとも同じ（brokerのcrateは`../../src`） | build scriptの実行に依るので作り直し | `FsStatusOutdated(StaleDepFingerprint)` |
| (e)でseedと同じcommit（追加の1回ずつ） | dirtyなし | fresh | fresh（`Compiling`が0。`build` 0.8秒、`test` 0.2秒、`cov` 0.4秒） |

- (e)では、絶対pathの食い違いは消えた。build scriptの`rerun-if-changed`が相対pathになり、cargoのfingerprintに入るpathと出力から読み直したpathが同じになったためである。
- それでも(e)で3つのbuild scriptが走り直したのは、`build_id::SOURCES`（`src`・`crates`・`migrations`・`build.rs`・`Cargo.toml`・`Cargo.lock`）を3つのcrateが同じ組で見張るからである。dagqの`src/`が変われば、brokerのcrateのbuild scriptも走り直し、`dagq-broker`と`dagq-broker-client`もcompileし直しになる。これはADR-t827-1（決定5・7）の、1つのcheckoutの3つのバイナリが同じbuildを名乗るための見張り方である。

## 4. freshかincrementalか

### 4.1 incrementalは、別のworktreeでは当たらない

小さなcrateで確かめた（`<scratch>/inc`、`RUSTC_BOOTSTRAP=1 rustc -Zincremental-info -C incremental=inc src/lib.rs`）。

- ディレクトリAでcompileし、`cp -c -Rp`でBに写してBで同じコマンドを流すと、rustcは`[incremental] completely ignoring cache because of differing commandline arguments`を出し、cacheを使わなかった。`-g`の有無に依らなかった。
- 同じディレクトリAで流し直すと、cacheを使った。
- A・Bの両方で`-Zremap-cwd-prefix=.`を付けると、Bでもcacheを使った（`session directory: 5 files hard-linked`、ignoringの行なし）。

rustcの`working_dir`（作業ディレクトリ）はincrementalの比べる引数に入る。cargoはworkspaceのcrateをworkspaceの根をcwdにしてcompileするので、worktreeが違えば必ずcacheが捨てられる。揃える手段（`-Zremap-cwd-prefix`、cargoの`trim-paths`）はどちらもstableでは使えない。llvm-covのbuild（`target/llvm-cov-target`）も同じcwdでcompileするので同じである。

### 4.2 結論

- **build scriptを直したとき、dagqのcrateがfreshになるのは、worktreeのcommitがseedと同じで、`SOURCES`のファイルが1つも変わっていないときだけである。** そのときは(e)のとおり全部freshになる。
- **runの変更かrebaseで`SOURCES`が1つでも変われば、3つのcrateはcompileし直しになり、incrementalにも当たらない。** 2章の(e)がこの場合で、(d)と同じだけかかった。
- workerの最初のbuildが「seedと同じcommitで変更の前」に当たればfreshになる。ただしそのworktreeのincrementalのcacheはseedのcwdのものなので、変更の後の最初のbuildはdagqのcrateを丸ごとcompileする。workerのbuildの合計は、依存crateの段の分しか縮まない。
- 着地の検証は、rebaseの後にrunの変更の上で流れる（[integrate](../design/supervisor-lifecycle/integrate.md#integrate)の5）。runの変更が`SOURCES`に1つも触れないrun（docsだけなど）でも、seedがbaseと同じcommitでなければrebaseで変わったファイルでbuild scriptが走り直す。着地の検証がfreshになる場合は、ほぼ無い。

### 4.3 worktree-seed.mdの3.2節（`cov`で(c)が短かった理由）

- incrementalの再利用ではない。4.1節のとおり、別のworktreeではrustcがcacheを捨てる。
- (c)と同じ形（seedと同じcommit、直していないbuild script、全ファイルのmtimeをseedと揃えたもの）を、低いloadで(a)と2回ずつ、c・a・a・cの順に流した（`cov`だけ）。

| 順 | 条件 | build（秒） | 依存crateの段（秒） | load1の平均 |
| --- | --- | --- | --- | --- |
| 1 | (c)と同じ形 | 95.7 | 0.3 | 12.6 |
| 2 | (a) | 82.0 | 6.6 | 9.7 |
| 3 | (a) | 77.6 | 4.9 | 8.1 |
| 4 | (c)と同じ形 | 88.0 | 0.4 | 9.5 |

- (c)と同じ形は、隣り合う(a)より13.7秒と10.4秒長く、task 968の56〜84秒の短縮は再現しなかった。(c)と同じ形でも`dagq`・`dagq-broker`・`dagq-broker-client`の3つがcompileし直しになった（理由はtask 968と同じ`RerunIfChangedOutputPathsChanged`）。
- 2章のunitの時間のとおり、cloneで縮むのはdagqのlibが始まるまでの時間（依存crate・`dagq-broker-protocol`・build scriptのcompile）だけで、その大きさは`build`・`test`の(a)で12〜49秒（中央値21秒と12秒）、`cov`の(a)で依存crateの段が7〜15秒である（`cov`はunitの時間を取っていない）。task 968の56〜84秒はこれより大きく、この測定の範囲では説明できない。task 968の`cov`のload1の平均は(a)が15.9〜27.5、(c)が14.4〜20.6で、3周目を除けば近く、load averageの差でも説明しきれない。理由は確かめきれなかった。確かめたのは、incrementalの再利用ではないことと、低いloadでは再現しないことである。

## 5. 1本あたりの短縮の見込みの見直し（worktree-seed.mdの4章から）

| 対象 | worktree-seed.mdの4章の見込み | この測定からの見込み | 根拠 |
| --- | --- | --- | --- |
| workerの最初のbuild | 約10〜20秒 | 約10〜20秒（build scriptを直しても変わらない） | (a)でdagqのlibが始まるまでの時間の中央値: `build` 21秒、`test` 12秒（(d)・(e)では1〜2秒）。依存crateの段だけなら`build` 15.5秒、`test` 10.5秒。dagqのcrateは(d)・(e)ともcompileし直し（4.2節） |
| 着地の検証のbuild（最初の試行） | 約10〜25秒 | 約15〜20秒（同上） | `cov`の依存crateの段の中央値13.4秒（7.2〜15.1秒）に、`build`・`test`でlibの開始が依存crateの段より遅れた分（中央値で2〜6秒）を足した見積もり。`cov`ではunitの時間を取っていない |
| cloneの時間（1 runに1回） | 2.3〜15.4秒（中央値6.4秒） | 2.4〜7.9秒（中央値3.9秒） | 2章 |
| seedを最新に保つbuild | 測っていない | 空から作り直すと着地ごとに約5分のbuild（`CARGO_BUILD_JOBS=4`の1本）。同じ場所で作り直す量は測っていない | この測定のseedのbuild（空のtargetから、壁時計）: 2つのseedで`build` 96秒と176秒、`test --no-run` 0.3秒と0.5秒、`cov` 195秒と123秒、合計292秒と299秒 |

差し引き:

- 1 runあたりの短縮は、workerと着地の検証を合わせて約25〜40秒、cloneの約4秒を引いて**約20〜35秒**である。runの作業と着地の数十分に比べて1〜2%で、loadの揺れ（同じ条件で2〜3倍）より小さい。
- seedのbuildの量は、作り直し方で大きく変わる。
  - 空から作り直す（この測定のseed）と、1回あたり約290〜300秒である。1回の作り直しの間に着地するrunが1本なら、1 runあたりの短縮（約20〜35秒）よりseedのbuildが大きく、hostのloadは増える。回収するには、1回の作り直しあたり約8〜15本のrunが新しいworktreeを作る必要がある。
  - 同じ場所のseedでmainを進めて作り直せば、依存crateはfreshのまま、dagqのcrateはincrementalに当たる（cwdが同じなので4.1節の理由で捨てられない）。量はこれよりずっと小さい見込みだが、この測定では測っていない。それでも着地ごとに`build`と`cov`のdagqのcrateのcompileが1本ずつ増える。
- 作り直しの間隔を伸ばすとseedが古くなる。`Cargo.lock`が変われば、変わった依存crateはcloneの後にcompileし直しになり（sccacheに当たればその分は短い）、短縮は小さくなる。
- build scriptを直すことは、この見込みを変えない（4.2節）。

## 6. 試作したbuild scriptの直し方と、正しさの見立て

### 6.1 直し方（使い捨てのcloneの中だけ。`src/`と`crates/`は変えていない）

1. `crates/dagq-broker-protocol/src/build_id.rs`の`emit`: `compute`が返す`cargo:rerun-if-changed=`の行のpathから`CARGO_MANIFEST_DIR`を外し、パッケージからの相対path（dagqは`src`、brokerのcrateは`../../src`など）で出す。
2. 同じfileの`git_state`: worktreeごとに違うGitのファイル（`git rev-parse --git-path`の`HEAD`・`index`・`packed-refs`と、`symbolic-ref`のbranchのref）を見張るのをやめた。commitと`.dirty`の判定は、build scriptが走るたびにこれまでどおり`git rev-parse`と`git status --porcelain`で行う。
3. `build.rs`の`embed_broker_material`: `rust-toolchain.toml`と、brokerのimageの材料の見張り（`broker_material::collect`の`watched`）を、`manifest_dir`を外した相対pathで出す。
4. `build.rs`の`list_migrations`: 生成する`migrations.rs`の`include_str!`の絶対pathを、`include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations/<file>"))`に変える。絶対pathのままだと、cloneしたtargetでbuild scriptが走らずdagqのlibだけをcompileし直すときに、seedのcheckoutのmigrationのfileを読み、dagqのlibのdep-infoもseedのfileを見張る。

### 6.2 正しさの見立て

- **相対pathにすること（1・3）と`CARGO_MANIFEST_DIR`から読むこと（4）は、見張る対象と埋め込む中身を変えない。** cargoは`rerun-if-changed`の相対pathをパッケージのディレクトリから解く。`include_str!`は同じfileを読む。brokerのimageの材料（`$OUT_DIR/broker-image.tar`）は、見張るfileが変わらなければcloneしたものが正しく、変われば走り直す。vendorされたcrate（rootがパッケージ自身の場合）も同じ相対pathの形で動く。
- **Gitのファイルを見張らないこと（2）は、build識別子を古くしうる。** build scriptは`SOURCES`のどれかが変わったときにしか走らないので、次の場合にbuild識別子が今のcommitと食い違う。
  - ソースを変えないcommit（docsだけのcommit、空のcommit、ソースが同じ別のbranchへのcheckout）の後のbuild。試作で、空のcommitの後の`cargo build`はbuild scriptを走らせず、`dagq --version`は1つ前のcommitを名乗った。
  - ソースを変えてbuildし（`.dirty`になる）、そのままcommitした後のbuild。build scriptは走らず、古いcommitの`.dirty`を名乗り続ける。
  - cloneしたtargetでも同じことが起きる。seedとworktreeのcommitの差が`SOURCES`の外（docsだけなど）だけなら、cloneしたtargetはfreshのままで、build識別子はseedのcommitを名乗る。provisioningでcloneする経路では、これがいちばん起きやすい。
  - 逆に、ソースを変えた後のbuildでは走り直すので、`.dirty`は正しく付く（試作で確かめた）。
  - 影響: build識別子は、バイナリが名乗るbuild（`dagq --version`。ADR-0045決定2）である。着地のcommitがソースを変えないとき（docsだけのtaskの着地など）、同じcheckoutで続けてbuildしたバイナリが前のcommitを名乗りうる。
- 試作は`build_id.rs`のunit test（`a_development_build_names_the_commit_of_its_root_and_whether_it_is_dirty`が`refs/heads/main`の見張りを確かめる）を直していない。本番で直すなら、そのtestも変わる。

### 6.3 Gitのファイルの見張りの代わりの案

- **(i) 共通のgit dirのfileを絶対pathで見張る。** cargoはパッケージの外のpathを絶対pathのまま記録し、同じrepositoryのworktreeはどれも共通のgit dir（`git rev-parse --git-common-dir`）が同じなので、その中のfile（`packed-refs`、`refs/heads/main`）は食い違わない。ただし、runのbranch（`dagq/<run>`）のrefやdetachedの`HEAD`・`index`はworktreeごとなので、worktreeの自分のcommitを追えない。正しさは2と同じ程度に弱い。
- **(ii) commitを環境変数で渡す。** runtimeが着地の検証・自動更新・`install`のbuildに、build識別子を`DAGQ_BROKER_IMAGE_BUILD`（`build_id::GIVEN_ENV`）のような変数で渡し、build scriptは`rerun-if-env-changed`で見張る。commitが変わるたびに走り直し、同じcommitならfreshのままになる。runtimeの変更が要り、渡さないbuild（人やworkerの手元）は2と同じ弱さになる。
- **(iii) build識別子を見張るbuild scriptを、大きなcrateから外す。** Gitを見張るbuild scriptを小さなcrateに分けても、cargoは依存のcrateが変われば依るcrateをcompileし直すので、dagqのlibは縮まない。binの`main.rs`だけが読む形にすれば、走り直してもlibは作り直さずに済むが、3つのバイナリの`DAGQ_BUILD_ID`の埋め込み方（ADR-t827-1決定5・7）を変える。
- どの案でも、runの変更とrebaseで`SOURCES`が変われば3つのcrateはcompileし直しになり、incrementalにも当たらない（4章）。build scriptの直し方で縮むのは、seedと同じcommitで変更の無いbuildだけである。

## 7. この測定で分からないこと

- 本番のloadの下での、条件ごとの数%の差。同じ条件の中で時間が2〜3倍揺れ、3周では分けられなかった。
- seedのbuildを着地ごとに足したときの、本番の着地の流れへの影響（測っていない。5章は時間の見積もりだけ）。
- `cargo clippy`の成果物のseed（worktree-seed.mdの8章から変わらない）。
