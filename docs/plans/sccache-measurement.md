---
id: plan-sccache-measurement
type: plan
title: sccache導入前後のintegrateのllvm-covの所要時間とhit率
status: completed
created: 2026-09-28
owners:
  - hisamekms
tags:
  - performance
  - build
  - measurement
related:
  - adr-0049
  - adr-0076
  - plan-nextest-measurement
---

# sccache導入前後のintegrateのllvm-covの所要時間とhit率

[ADR-0049](../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定10の「導入後」の測定（task 460）。goal 36（並列数を上げて得をできるようにする）のacceptance (1)(4)の材料で、この文書はtaskを登録しない。比較は人の決定（2026-09-26、ask 100・101の(a)）どおり期間AとCで行う。

## 要点

- **integrateのllvm-covの段の全体は縮んでいない。** 主な比較（`--parallel 3`のAとC）で中央値は263.5秒→272秒（範囲165〜390→214〜474）。buildは74秒→39秒に縮んだが、test段が163.5秒→220秒に伸びて打ち消した。
- **integrateのllvm-covのbuildはsccacheがほとんど効かない形だった。** 47 runのうち38 runは`dagq` crate 1つだけをcompileしていた（依存crateはrunのtargetにbuild済み）。workspaceのcrateはsccacheの対象外（`incremental`）なので、この38 runのbuildの短縮（A3 74秒→C 35秒）はsccacheではなく、toolchainのarm64化と負荷の差による。
- **依存crateをcompileしたrun（A4で5本、A3で0本、Cで4本）では差が出た。** 依存をcompileした分の上乗せ（同じ期間の`dagq`だけのrunとのbuildの中央値の差）は、A（`--parallel 4`）で約81秒、Cで約17秒。arm64化だけなら約3分の1（約27秒）になる見込みなので、残りの約10秒がsccacheに帰せる分の上限の目安。1着地あたりにならすと数秒で、llvm-covの段の揺れ（±50秒以上）に埋もれる。
- **sccacheのhitは多い。** 測定時のserver（2026-09-27 08:57 JSTから約20.8時間）で、compile requests 31,941、executed 18,869、hits 16,081、misses 2,369、Rustのhit率87.08%。この間にclaimされたrunは120本で、1 runあたり約130〜150件のhitになる。hitの多くはworkerの最初のbuild（新しいworktreeで依存crateをcompileする`cargo test`・`clippy`・`llvm-cov`）で出ており、integrateのllvm-covではない。
- **workerの作業（claim→receipt）とstartupは大きく縮んだが、sccacheだけの効果とは言えない。** llvm-covを含むrunのworkの中央値は2,333秒→1,340秒、全runのstartupは1,974.5秒→707秒、wait_to_landは685秒→526秒。同じ時期にtoolchainのarm64化（release buildが約3分の1）と`CARGO_BUILD_JOBS=4`が入っており、hit数だけからrunごとの短縮秒数は切り分けられない。
- **sccacheのserverは`/exit`の確認画面を出していない。** 導入後の最初のrun（task 427）からC期間の終わりまで、すべての`exit_requested`が1〜2秒で`session_exited`になり、`exit_request_timed_out`と`stuck_exit`のaskは0件（決定6の見込みどおり）。
- **並列数を上げる判断の材料**: 依存crateのcompileはsccacheでほぼ無くなったので、runを1本増やしたときのCPUの増分はworkspaceのcrateのcompile・link・testの実行に絞られた。integrateのllvm-covの律速はtest段（testの合計÷並列度、[nextest-measurement](nextest-measurement.md)）で、sccacheはそこに効かない。並列数を上げるかどうかは、sccacheではなく、testの並列度とloadで判断するのがよい。

## 1. 期間の境界

| 項目 | 値 |
| --- | --- |
| S（sccacheを有効化） | task 393、commit `f76cc30`、main上の時刻2026-09-26 11:46:49 JST。`run_integrated`はevent 10372（02:46:49Z） |
| P（`CARGO_BUILD_JOBS=4`・`RUST_TEST_THREADS=4`） | task 427、commit `6fef4da`、2026-09-26 11:49:24 JST。`run_integrated`はevent 10408（02:49:24Z） |
| `--parallel` 4→3 | 2026-09-26 08:02 JST（2026-09-25 23:02Z）、cursor 8806。note 8810のとおり、cursor 8739でbinary 3921f21・parallel 4のsupervisorが起動し、すぐに3に入れ替えた。`backend_call_failed`の`parallel`も23Z台から3 |
| toolchainのarm64化 | 2026-09-26 11:48 JSTごろ（人の記録。task の記述）、cursor 10319（event 10319は11:41:52 JST）。実際の切り替わりはarm64のtoolchainが入った11:37〜11:38 JSTから11:48 JSTの間で、どの点を取ってもA3の後・Cの前に入る。`~/.rustup/toolchains/*-aarch64-apple-darwin`は11:37〜11:38 JSTに入った。A3の最後のrun（task 395、llvm-covの段は11:31 JST開始）までx86_64（Rosetta） |
| 期間A4 | 2026-09-25 21:00 JST（12:00Z、event 6815）から`--parallel`の切り替え（cursor 8806）まで。`--parallel 4`、x86_64、sccacheなし |
| 期間A3 | cursor 8806からS（event 10372、S自身は含めない）まで。`--parallel 3`、x86_64、sccacheなし。**主な比較に使うA** |
| 期間B | SとPの間（event 10372〜10408）。着地は**0 run**（S・Pの2本自身を除く）。比べない |
| 期間C | P（event 10408、P自身は含めない）から`884b3d5`（task 518、event 11911、2026-09-26 15:50:05 JST、518自身は含めない）まで。`--parallel 3`、arm64、sccacheあり、`CARGO_BUILD_JOBS=4`・`RUST_TEST_THREADS=4` |

Cの終わりを`884b3d5`で切ったのは、integrateのcoverageの関門が`cargo llvm-cov nextest`に変わった（task 518、ADR-0076）ためである。Cの15 runはどれも旧コマンド`cargo llvm-cov --locked --fail-under-lines 80`で流れ、nextestで流れたrunは含まない（切り替え後の比較は[nextest-measurement](nextest-measurement.md)が行った）。A4の始まりは、A3と同程度以上のrun数を`--parallel 4`で取るために前日の21:00 JSTに置いた。

### Cに重なる変更（AとCの差をそのままsccacheの効果と読まない）

1. **`CARGO_BUILD_JOBS=4`・`RUST_TEST_THREADS=4`（task 427、`6fef4da`）**: Pの直後から効く。buildの並列度を絞るのでrun単体のbuildは遅くなりうる一方、hostの取り合いは減る。`RUST_TEST_THREADS`はAでは未設定（testのbinaryの中の既定の並列度はコア数の8）で、Cでは4になったので、**test段が伸びた一因**になる。
2. **`--parallel` 4→3（2026-09-26 08:02 JST、cursor 8806）**: A3は既に3なので、主な比較（A3対C）には入らない。A4対Cには入る。
3. **toolchainのx86_64（Rosetta）→arm64（2026-09-26 11:48 JSTごろ、cursor 10319）**: release buildが2分03秒〜2分15秒→43秒（約3分の1）。Cの全runに効き、workspaceのcrateのcompile・link・testの実行のすべてを速める。
4. **coverageの関門の`cargo llvm-cov nextest`への切り替え（task 518、`884b3d5`）**: Cの終わりで切ったので、Cのllvm-covの段には入らない。
5. そのほか、期間の間にtestが増え、testのファイルの分割でtestのbinaryがA3の11本からCの25〜46本に増えた（link・起動の時間が増える）。task 528（06:30Z着地、workerが全体の`cargo test`を流さなくなった）はCの終わりの近くで着地し、Cでclaimされたllvm-covのrunには効いていない。

## 2. integrateのllvm-covの段

llvm-covの段の時間は、同じintegrateの試行の直前のコマンド（clippy）の`verification_command`のeventとllvm-covのeventの時刻の差で出した（logの`integrate-<attempt>-verify-3.log`の作成時刻からの差とも一致した）。runごとに最後の試行を使った。buildはlogの最後の``Finished `test` profile ... in``、test段はbinaryごとの`finished in`の合計、「後」は段からbuildとtest段を引いた残り（profrawのmergeとreport）。`deps`はlogの`Compiling`の行数で、1は`dagq`だけ、66は依存crateも含む。

| 期間 | run数 | llvm-covの段 中央値（範囲） | build | test段 | 後 | clippy | `land_phases.verify` | testのbinary |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| A4（parallel 4） | 22 | 277秒（154〜682） | 94.5秒（38〜446） | 147秒（99〜245） | 14秒（8〜25） | 21秒（0〜208） | 324.5秒（155〜746） | 9〜10 |
| A3（parallel 3） | 10 | 263.5秒（165〜390） | 74秒（39〜128） | 163.5秒（115〜272） | 16.5秒（11〜26） | 20秒（9〜82） | 281.5秒（183〜435） | 11 |
| B | 0 | — | — | — | — | — | — | — |
| C | 15 | 272秒（214〜474） | 39秒（24〜125） | 220秒（177〜318） | 18秒（5〜31） | 8秒（0〜35） | 302秒（219〜726） | 11〜46 |

依存crateをcompileしたかどうかで分けると次のとおり。

| 期間 | 区分 | run数 | build 中央値（範囲） | llvm-covの段 中央値（範囲） |
| --- | --- | --- | --- | --- |
| A4 | 依存も（66） | 5 | 169秒（107〜446） | 354秒（242〜682） |
| A4 | `dagq`だけ（1） | 17 | 88秒（38〜199） | 276秒（154〜376） |
| A3 | 依存も（66） | 0 | — | — |
| A3 | `dagq`だけ（1） | 10 | 74秒（39〜128） | 263.5秒（165〜390） |
| C | 依存も（66） | 4 | 52.5秒（38〜67） | 304秒（265〜353） |
| C | `dagq`だけ（1） | 11 | 35秒（24〜125） | 265秒（214〜474） |

runごとの値（時刻はllvm-covの段の開始、UTC。loadは`metrics.csv`の段の間の`load1`の平均／最大で、2026-09-26 10:53 JSTより前は記録が無い）:

| 期間 | task | run | 開始 | 段 | build | deps | test段 | 後 | clippy | `land_phases.verify` | work | wait_to_land | load1 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| A4 | 277 | 6eabc898 | 09-25 12:16 | 160 | 52 | 1 | 99 | 9 | 14 | 189 | 3429 | 1988 | — |
| A4 | 308 | 0b186ea1 | 12:28 | 220 | 88 | 1 | 119 | 13 | 9 | 231 | — | 803 | — |
| A4 | 276 | 2c7b6ff0 | 13:47 | 278 | 135 | 1 | 128 | 15 | 208 | 662 | 1735 | 1737 | — |
| A4 | 288 | 24f6fef5 | 13:52 | 167 | 49 | 1 | 109 | 9 | 28 | 196 | 3061 | 2043 | — |
| A4 | 247 | 05255cc7 | 13:56 | 179 | 43 | 1 | 125 | 11 | 20 | 199 | 4485 | 473 | — |
| A4 | 246 | b36ceaf3 | 14:11 | 242 | 125 | 66 | 109 | 8 | 21 | 269 | 865 | 292 | — |
| A4 | 337 | 8396e0c4 | 14:15 | 173 | 47 | 1 | 118 | 8 | 17 | 191 | 4080 | 1772 | — |
| A4 | 180 | fedff397 | 14:44 | 682 | 446 | 66 | 211 | 25 | 58 | 746 | 1643 | 779 | — |
| A4 | 326 | 6c3b865e | 15:04 | 302 | 90 | 1 | 192 | 20 | 11 | 315 | 2319 | 737 | — |
| A4 | 281 | 5cde3fd8 | 15:28 | 347 | 199 | 1 | 134 | 14 | 21 | 369 | 3665 | 2442 | — |
| A4 | 339 | 9acf0c9a | 15:34 | 251 | 107 | 66 | 133 | 11 | 21 | 273 | 1118 | 630 | — |
| A4 | 346 | 4c4429e7 | 15:53 | 437 | 257 | 66 | 160 | 20 | 26 | 464 | 972 | 506 | — |
| A4 | 376 | 6ac64bc4 | 16:02 | 354 | 169 | 66 | 171 | 14 | 21 | 377 | 1320 | 410 | — |
| A4 | 282 | 368fdfcd | 16:31 | 348 | 175 | 1 | 155 | 18 | 0 | 349 | 2706 | 1038 | — |
| A4 | 260 | 4d05557f | 16:41 | 213 | 56 | 1 | 143 | 14 | 47 | 262 | 2363 | 298 | — |
| A4 | 297 | 3c9b843f | 16:46 | 322 | 132 | 1 | 170 | 20 | 40 | 364 | 2149 | 413 | — |
| A4 | 321 | b99b79eb | 17:42 | 254 | 90 | 1 | 151 | 13 | 12 | 462 | 1080 | 1922 | — |
| A4 | 293 | 12cb6c33 | 18:26 | 280 | 99 | 1 | 168 | 13 | 1 | 574 | 2036 | 1708 | — |
| A4 | 322 | ff7a4aa3 | 18:46 | 285 | 52 | 1 | 218 | 15 | 22 | 309 | 1084 | 327 | — |
| A4 | 331 | eab1ba08 | 19:41 | 376 | 108 | 1 | 245 | 23 | 0 | 378 | 1821 | 400 | — |
| A4 | 333 | daee7501 | 20:00 | 154 | 38 | 1 | 106 | 10 | 0 | 155 | 2606 | 525 | — |
| A4 | 328 | 8b5ed3af | 22:50 | 276 | 62 | 1 | 197 | 17 | 57 | 334 | 14266 | 355 | — |
| A3 | 338 | 1dc0d896 | 23:17 | 304 | 128 | 1 | 159 | 17 | 82 | 398 | 2333 | 28038 | — |
| A3 | 361 | 1e82a5e7 | 23:26 | 165 | 39 | 1 | 115 | 11 | 18 | 183 | — | 215 | — |
| A3 | 324 | 91447cb6 | 23:43 | 302 | 100 | 1 | 182 | 20 | 22 | 326 | 2247 | 16772 | — |
| A3 | 360 | 7fdac081 | 23:49 | 205 | 50 | 1 | 143 | 12 | 16 | 222 | 2580 | 19506 | — |
| A3 | 292 | 37b3687d | 09-26 00:04 | 352 | 117 | 1 | 209 | 26 | 45 | 426 | 2630 | 23248 | — |
| A3 | 353 | d380f59d | 00:34 | 337 | 124 | 1 | 194 | 19 | 33 | 374 | 1214 | 391 | — |
| A3 | 354 | 5bf44a1a | 01:08 | 225 | 47 | 1 | 162 | 16 | 11 | 237 | 2416 | 673 | — |
| A3 | 309 | 15719c13 | 01:32 | 390 | 98 | 1 | 272 | 20 | 42 | 435 | 3009 | 464 | — |
| A3 | 355 | a0eb397f | 01:47 | 221 | 44 | 1 | 165 | 12 | 9 | 231 | 1957 | 697 | — |
| A3 | 395 | d429aad0 | 02:31 | 200 | 47 | 1 | 142 | 11 | 9 | 211 | 2311 | 262 | 5.8／7.3 |
| C | 345 | 54a52f8b | 02:58 | 231 | 28 | 1 | 196 | 7 | 0 | 232 | — | 775 | 12.3／16.3 |
| C | 403 | abda1589 | 03:19 | 214 | 32 | 1 | 177 | 5 | 5 | 219 | 954 | 244 | 7.9／10.3 |
| C | 335 | a7554f40 | 03:33 | 320 | 39 | 1 | 261 | 20 | 8 | 329 | 1415 | 1424 | 13.0／18.0 |
| C | 394 | 96a9f439 | 03:46 | 264 | 24 | 1 | 224 | 16 | 7 | 271 | 1463 | 728 | 10.0／15.8 |
| C | 310 | 5575d5aa | 04:16 | 259 | 35 | 1 | 208 | 16 | 7 | 428 | 811 | 962 | 9.8／14.6 |
| C | 358 | ecc79d4f | 04:21 | 276 | 40 | 1 | 220 | 16 | 26 | 302 | 1889 | 922 | 10.1／13.5 |
| C | 466 | 9498021b | 04:49 | 474 | 125 | 1 | 318 | 31 | 26 | 501 | 1658 | 526 | 23.6／29.5 |
| C | 510 | 40aa57cd | 04:58 | 342 | 71 | 1 | 249 | 22 | 35 | 726 | 787 | 1478 | 17.2／22.0 |
| C | 490 | de8c2c19 | 05:15 | 248 | 26 | 1 | 206 | 16 | 7 | 255 | 1015 | 281 | 6.1／6.9 |
| C | 461 | ebe5c2dd | 05:26 | 306 | 64 | 1 | 223 | 19 | 8 | 462 | 1499 | 5239 | 10.8／18.5 |
| C | 357 | ac231167 | 05:31 | 336 | 67 | 66 | 251 | 18 | 15 | 352 | 1265 | 429 | 12.5／16.8 |
| C | 491 | efcff4e9 | 05:44 | 265 | 38 | 66 | 211 | 16 | 11 | 276 | 744 | 297 | 6.4／10.4 |
| C | 382 | 799c6d99 | 05:49 | 265 | 27 | 1 | 217 | 21 | 12 | 277 | 1700 | 329 | 5.4／8.8 |
| C | 492 | b013fe1b | 06:02 | 272 | 39 | 66 | 215 | 18 | 7 | 280 | 705 | 304 | 5.1／7.5 |
| C | 462 | c321a245 | 06:12 | 353 | 66 | 66 | 258 | 29 | 11 | 366 | 2030 | 390 | 10.3／16.6 |

workが「—」のrunは`receipt_observed`の無いrun（adoptやresumeで受理されたもの）。

## 3. 作業（claim→receipt）・startup・wait_to_land

`dagq stats --full`（固定バイナリ、測定時の`next_cursor`以前の全run）のrunの行を、着地時刻で期間に分けて中央値を取った（S・P自身は含めない）。`work`は`run_claimed`→最初の`receipt_observed`、`wait_to_land`は最初の`validation_finished`→`run_integrated`。

| 期間 | 着地run数 | work 中央値（範囲） | うちllvm-covのrun | startup 中央値 | wait_to_land 中央値（範囲） | うちllvm-covのrun |
| --- | --- | --- | --- | --- | --- | --- |
| A4 | 33 | 1,239秒（178〜14,266） | 2,149秒（21 run） | 1,097.5秒 | 473秒（16〜2,442） | 683.5秒 |
| A3 | 13 | 2,322秒（526〜4,403） | 2,333秒（9 run） | 1,974.5秒 | 673秒（22〜28,038） | 685秒 |
| B | 0 | — | — | — | — | — |
| C | 23 | 882.5秒（186〜2,030） | 1,340秒（14 run） | 707秒 | 304秒（14〜5,239） | 526秒 |

Cは着地run 23本のうちdocs・pluginなどllvm-covを含まないrunが8本（A3は3本）あり、全runの中央値はその分だけ短く出る。runの種類をそろえたllvm-covのrunの列で比べる。A3のwait_to_landの上限（28,038秒）は夜間の人の判断待ちを含む。

## 4. load average

| 期間 | 源 | 値 |
| --- | --- | --- |
| A4 | `backend_call_failed`の`load_avg`（cmuxの時間切れのときだけ記録） | 252件、中央値47.5（18.5〜131.0） |
| A3 | 同上 | 3件、中央値40.8（31.3〜49.2） |
| C | 同上 | 0件 |
| A3の終わり（10:53〜11:46 JST） | `~/.local/share/dagq-hostmetrics/metrics.csv`の`load1`（約30秒ごと、101点） | 平均9.6、中央値7.6、最大24.2 |
| C（11:49〜15:50 JST） | 同上（455点） | 平均10.4、中央値9.4、最大31.0 |
| C | 同上、各runのllvm-covの段の平均 | 中央値10.1（5.1〜23.6）、最大29.5 |

A4とA3の大半は`metrics.csv`の記録が始まる前（2026-09-26 10:53 JSTより前）で、runtimeもrunの`load`をまだ記録していなかった。`backend_call_failed`の`load_avg`は時間切れが起きたときだけの偏った標本なので、loadの水準ではなく、時間切れの件数の差（A4 252件→A3 3件→C 0件）として読む。参考に、note 8718（`--parallel 4`、旧バイナリの区間）の`stats`の`max_load_avg`は直近50 runで151、全199 runで204。CとA3の終わりの`metrics.csv`は同じ水準（平均9.6と10.4）で、Cでloadが下がったとは言えない。

## 5. sccacheの`--show-stats`

測定時点（2026-09-28 05:46 JST）のhostの既定のcache（`~/Library/Caches/Mozilla.sccache`、sccache 0.18.0）の値。serverは2026-09-27 08:57:09 JSTに起動したもの（`ps`の`lstart`）で、統計はserverの起動からの累計なので、C期間（2026-09-26）の値は残っていない。導入直後の`--show-stats`はtask 393の時点で記録されておらず、導入直後との差分は取れない。

| 項目 | 値 |
| --- | --- |
| Compile requests | 31,941 |
| Compile requests executed | 18,869 |
| Cache hits | 16,081（Rust 15,969、C/C++ 112） |
| Cache misses | 2,369 |
| Cache hits rate | 87.16%（Rust 87.08%） |
| Compilation failures | 419 |
| Non-cacheable calls | 12,608（`crate-type` 4,068、`incremental` 2,535、`multiple input files` 2,407、`-` 1,338、`missing input` 1,214、`missing output_dir` 632、`-o` 414） |
| Average compiler | 0.936秒 |
| Cache size | 4 GiB（上限10 GiB） |

同じserverの間（約20.8時間）にclaimされたrunは120本（着地はruntime 59本、kindなし46本、docs 7本、plugin 5本、ci 1本）。

### hit数で切り分けられる分とそうでない分

- **切り分けられる分**: sccacheが省いたのは依存crateのrlibのcompileで、この20.8時間でhit 16,081件。missの平均compile時間0.936秒で見積もると、約4.2 CPU時間（hostの8コアのうち平均約0.2コア分）のcompileを省いた。1 runあたりでは約130〜150件のhit（ADR-0049のContextの測定では、1種のbuildで依存crateの57/64件がhitする。workerの`cargo test`・`clippy`・`llvm-cov`とintegrateのllvm-covのうち、依存をbuildするものの数だけ積み上がる）。
- **integrateのllvm-covへの効き**: Cの15 runのうち依存crateをcompileしたのは4 runで、依存の分の上乗せはA4の約81秒からCの約17秒に減った。arm64化だけの見込み（約3分の1で約27秒）との差の約10秒がsccacheに帰せる上限の目安で、4/15 runにしか起きないので1着地あたりでは数秒になる。残りの11 runは`dagq`だけのcompileで、sccacheは効いていない（`incremental`で対象外）。
- **切り分けられない分**: workerのwork（2,333秒→1,340秒）とstartup（1,974.5秒→707秒）の短縮。hitはrunごとに記録されず、同じ時期にtoolchainのarm64化と`CARGO_BUILD_JOBS=4`が入ったので、どれだけがsccacheによるかは言えない。integrateのllvm-covのbuildの`dagq`だけの部分（74秒→35秒）とclippy（20秒→8秒。integrateのclippyは`dagq`の1 crateだけをcheckする）の短縮はsccacheの対象外で、arm64化と負荷の差による。test段の伸び（163.5秒→220秒）はtestの追加、binaryの数の増加（11本→25〜46本）、`RUST_TEST_THREADS=4`（task 427）による。
- **integrateがsccacheを通ったことの確かめ**: integrateのverifyのlogにはsccacheの痕跡が出ない（`RUSTC_WRAPPER`は表示されない）ので、C期間にintegrateのllvm-covがsccacheを通ったことはlogからは確かめられない。Cの依存をcompileした4 runのbuild（38〜67秒）が`dagq`だけのrun（中央値35秒）に近いことは、依存がhitしたことと矛盾しない。

## 6. sccacheのserverと`/exit`の確認画面（決定6）

導入後（event 10372より後）の最初のrunはtask 427（run `6a36f2aa`、02:46:53Zにclaim）で、`exit_requested`（02:49:19Z）の1秒後に`session_exited`（exit code 0）になった。依存crateをsccache経由でcompileした最初のruntimeのrunはtask 335（run `a7554f40`、02:51:31Zにclaim）で、`exit_requested`（03:31:07Z）の2秒後に`session_exited`。C期間（event 10408〜11911）の`exit_requested`はすべて1〜2秒で`session_exited`になり、`exit_request_timed_out`は0件、`stuck_exit`のaskも0件（この間に開いたaskはworker_question 100、decide 101・103、approve_landing 102、planner_question 104だけ）。sccacheのserverはclientから切り離されたdaemonで、Claude Codeの「Background work is running」の確認画面の原因になっていない。

## 7. 所見と並列数の判断への材料

- ADR-0049決定10の問い「有効にした前後でintegrateの検証の所要時間が縮んだか」への答えは「llvm-covの段では縮んでいない（263.5秒→272秒）」。integrateのllvm-covは多くのrunで依存crateをcompileしないので、sccacheの効く余地がもともと小さかった。
- sccacheが効くのはworkerの最初のbuildで、hitは1 runあたり130〜150件。runを1本増やしたときの依存crateのcompile（A4のintegrateのllvm-covでは依存の分がbuildに約81秒）はほぼ無くなり、並列数を上げたときのCPUの増分はworkspaceのcrate・link・testの実行に絞られた。goal 36のacceptance (1)の「共有の前後でrunのbuild時間が縮んだ」は、integrateのllvm-covのbuild（74秒→39秒）とworkerのwork・startupの短縮で数字の上では示せるが、その多くはtoolchainのarm64化と重なっており、sccache単独の寄与は上の切り分けのとおり小さい。
- 並列数を上げる判断は、sccacheよりも、integrateのtest段（testの合計÷`NEXTEST_TEST_THREADS`、[nextest-measurement](nextest-measurement.md)と[nextest-test-threads](nextest-test-threads.md)）、load（Cで平均10.4・最大31.0、8コア）、cmuxの時間切れの件数（A4 252件→A3 3件→C 0件）で見るのがよい。`--parallel 4`のA4では時間切れが多発しており、3に下げた後は0件に近い。
- 測り直すなら、sccacheの`--show-stats`をintegrateの前後で取る（または`sccache --zero-stats`をしてから一定時間の値を取る）ことで、integrateとworkerのhitを分けて数えられる。今のhostでは`--zero-stats`は他のrunの統計も消すので、この測定ではしていない。
