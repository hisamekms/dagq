---
id: plan-container-oom-detection
type: plan
title: スパイク：container の中の process の OOM kill を runtime が確かな事実として読める方法（--rm との順序・子 process だけの kill・container の上限と machine 全体の OOM の区別）のコードと公式資料の調べと、実物の podman で確かめる手順
status: active
created: 2026-10-23
owners:
  - hisamekms
tags:
  - planning
  - security
related:
  - plan-container-runtime-spike
  - adr-t2113-3
  - adr-t2257-1
  - adr-t1487-1
  - adr-t3163-1
---

# スパイク：container の中の process の OOM kill を runtime が確かな事実として読める方法

問いは「container の中の子 process が OOM killer に殺されたことを、runtime が後から確かな事実として、container の上限によるものか machine 全体のメモリ不足によるものかを分けて読めるか」。
答えが、OOM kill を verification の失敗と取り違えない task の形を決め、上限を重ねる形を見直す条件に当たるかを示す（下の「結果ごとの後続の task の形」）。
同時に動く container の数の抑え方は、上限を重ねて OOM を分類に任せる案 (B) を外し、machine のメモリから上限を予約する形に [ADR-t3163-1](../adr/2026-10-25-t3163-1-containers-reserve-memory-on-the-machine.md) で決めたので、この答えを待たない。

- コードと公式資料の調べ（下の (1)〜(3)）は worker が行い、出典は版と commit の permalink で示す。
  コードと資料で決まらない点は「実物で確かめる」と書き、どの場合（C1〜C5）で決まるかを添える。
- 実物の確かめは人か inbox が流し、worker は流さない（worker は host の podman と machine を使えない。[運用](../development/operations.md)の「workerがhostと実queueでできないこと」）。
  手順は [container-runtime-spike.md](container-runtime-spike.md) の (b)〜(d) と同じ形で、Spike の machine `dagq-spike` を使う。
- 調べたときの版: Podman v6.1.4（`464c15fec06f0074c916f9eff87c4806869c2833`）、conmon v2.2.1（`c8cc2c4db27531bd4e084ce7857f73cd21ee639d`）、crun 1.30.1（`079ff6a7a16d029460af882f9ea44674e6a510b6`）、Linux v6.17（`e5f0a698b34ed76002dc5cff3804a61c80233a7a`）。
  machine の中の版は実物の表の頭の comment に script が残す。

## 今の container の形

- 制御側は worker の turn と verification のどちらの container にも `--rm` を付ける（[run_container.rs の hardened](../../src/domain/run_container.rs)）。
  verification はコマンドごとに 1 つの container で、主の process は `dagq relay verify`、検証のコマンドはその子の `sh -c`、rustc などは孫になる（`verification_command`）。
- `dagq relay verify` の exit code は子の `sh -c` の exit code で、signal で死んだときだけ 128＋signal になる（[container_turn.rs の run_verification](../../src/infrastructure/container_turn.rs)）。
  孫が OOM で殺されても、cargo のような親が自分の失敗の code（例: 101）で終われば、container の exit code に OOM の跡は残らない。

## (1) State.OOMKilled の決まる箇所と時点と、--rm の削除との順序

判定: 成立（コードで決まる）。

- conmon は container の init の pid の cgroup（`/proc/<pid>/cgroup` の v2 の行）の `memory.events` に inotify の `IN_MODIFY` を掛ける（[cgroup.c L110-L140](https://github.com/containers/conmon/blob/c8cc2c4db27531bd4e084ce7857f73cd21ee639d/src/cgroup.c#L110-L140)、呼ぶのは [conmon.c L418](https://github.com/containers/conmon/blob/c8cc2c4db27531bd4e084ce7857f73cd21ee639d/src/conmon.c#L418)）。
- 通知のたびに `memory.events` の `oom` と `oom_kill` を読み、どちらかが 0 でなく前に見た値と違えば persist の dir と bundle の dir に `oom` のファイルを作る（[cgroup.c L281-L350](https://github.com/containers/conmon/blob/c8cc2c4db27531bd4e084ce7857f73cd21ee639d/src/cgroup.c#L281-L350)、[L356-L375](https://github.com/containers/conmon/blob/c8cc2c4db27531bd4e084ce7857f73cd21ee639d/src/cgroup.c#L356-L375)）。
  `oom` と `oom_kill` を区別せず、どちらの変化でも同じ 1 つのファイルになる。
- `oom` は割り当てが上限で失敗しかけた回数で、kill の数ではない（[cgroup-v2 の memory.events](https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html#memory-interface-files)）。
  kernel は `oom` を OOM killer を呼ぶ前に数え（[memcontrol.c L1660-L1667](https://github.com/torvalds/linux/blob/e5f0a698b34ed76002dc5cff3804a61c80233a7a/mm/memcontrol.c#L1660-L1667)）、呼んだ先でも、待ちが signal で切れたときや、その間にメモリが空いて余裕が戻ったときは誰も殺さない（[memcontrol.c L1639-L1643](https://github.com/torvalds/linux/blob/e5f0a698b34ed76002dc5cff3804a61c80233a7a/mm/memcontrol.c#L1639-L1643)）。
  そのため `State.OOMKilled` の true は「container の cgroup で OOM の事態（上限での割り当ての失敗しかけか、OOM killer の kill）が起きた」ことまでしか言わず、process が殺されたことは言わない。
  kill を言うには `oom_kill` の増えの根拠が要る（M2・M4 が読む値か、答え合わせの M6）。
- 変化を見つけなかった通知（`max` や `high` だけの変化）では callback が `G_SOURCE_REMOVE` を返し、inotify の watch を外す（[cgroup.c L183-L212](https://github.com/containers/conmon/blob/c8cc2c4db27531bd4e084ce7857f73cd21ee639d/src/cgroup.c#L183-L212) と L349）。
  kernel は `max` を reclaim の前、OOM より先に数えるので、上限に達する OOM（C1 も）の多くは watch では拾えない見込みで、拾えるのは callback が OOM の後に読んだときだけ。
- ただし conmon は container が終わって main loop を抜けた後に `check_cgroup2_oom()` をもう 1 回呼び、その後で exit のファイルを書く（[conmon.c L470](https://github.com/containers/conmon/blob/c8cc2c4db27531bd4e084ce7857f73cd21ee639d/src/conmon.c#L470)、exit のファイルは [L514-L520](https://github.com/containers/conmon/blob/c8cc2c4db27531bd4e084ce7857f73cd21ee639d/src/conmon.c#L514-L520)）。
  そのとき cgroup の dir が残っていれば、watch が外れていても終わりの値で `oom` のファイルができる。
  dir が消えていれば（ENOENT）何もしない（[cgroup.c L296-L303](https://github.com/containers/conmon/blob/c8cc2c4db27531bd4e084ce7857f73cd21ee639d/src/cgroup.c#L296-L303)）。
  終わりの時点で dir が残っているかはコードで決まらず、実物で確かめる（C1・C4 の State.OOMKilled と died の欄）。
- libpod は exit のファイルを見つけたとき（`checkExitFile`）に `handleExitFile` で exit code を読み、persist の dir の `oom` のファイルが在れば `State.OOMKilled` を true にし、同じ時点で died の event を書く（[container_internal.go L199-L226](https://github.com/containers/podman/blob/464c15fec06f0074c916f9eff87c4806869c2833/libpod/container_internal.go#L199-L226)・[L2708-L2736](https://github.com/containers/podman/blob/464c15fec06f0074c916f9eff87c4806869c2833/libpod/container_internal.go#L2708-L2736)、`oom` のファイルの path は [oci_conmon_common.go L850-L854](https://github.com/containers/podman/blob/464c15fec06f0074c916f9eff87c4806869c2833/libpod/oci_conmon_common.go#L850-L854)）。
  conmon は `oom` のファイルを exit のファイルより先に作るので、両者の間に取り違えの競合は無い。
- died の event は `State.OOMKilled` が true のときだけ `OOMKilled` の欄を持つ（[events.go L99-L118](https://github.com/containers/podman/blob/464c15fec06f0074c916f9eff87c4806869c2833/libpod/events.go#L99-L118)）。
  container の種類の event に oom は無く、`OOMKilled` は `--format` の欄としてだけ読める。
  event は `events_logger` の journald（既定）か file に残り、`--since` で後から読める（[podman-events](https://docs.podman.io/en/latest/markdown/podman-events.1.html)）。
- `podman run` は container の exit code を待ち、`--rm` なら続けて container を消す（`--connection` で使う remote の client は [tunnel/containers.go L938-L953](https://github.com/containers/podman/blob/464c15fec06f0074c916f9eff87c4806869c2833/pkg/domain/infra/tunnel/containers.go#L938-L953)、local は [abi/containers.go L1260-L1263](https://github.com/containers/podman/blob/464c15fec06f0074c916f9eff87c4806869c2833/pkg/domain/infra/abi/containers.go#L1260-L1263)、[podman-run の --rm](https://docs.podman.io/en/latest/markdown/podman-run.1.html#rm)）。
  `podman run` の exit code は container の exit code だけで、OOMKilled を運ばない。
  `--rm` では `podman run` が戻った後の `podman inspect` は container を見つけられず `State` を読めないが、died の event は削除の前に書かれて残る。

## (2) 方法ごとの成り立ち

### (M1) --rm を外し、wait の後に inspect の State.OOMKilled を読んでから rm する

判定: OOM の事態を読むことは成立、kill の断定と、上限と machine 全体の区別は不成立。

- `State.OOMKilled` は (1) のとおり exit の処理の時点で決まるので、`--rm` を外せば `podman wait` の後の `podman inspect` で読める。
- 中身は conmon の `oom` のファイルで、`oom` と `oom_kill` を区別しない（[cgroup.c L317-L346](https://github.com/containers/conmon/blob/c8cc2c4db27531bd4e084ce7857f73cd21ee639d/src/cgroup.c#L317-L346)）ので、上限による OOM と machine 全体の OOM を分けない。
- 費用: 全ての経路（成功・失敗・時間切れの stop・supervisor の落ち・制御側の error）で `podman rm` が要る。
  消し損ねると、次に同じ名前（verification と run から決まる名前）で作るときに名前の重複で起動できない。
  同じ記録は (M5) が `--rm` のまま読めるので、M1 を採る理由は無い。

### (M2) 主の process（dagq relay verify・dagq relay turn）が中から memory.events を読んで差分を返す

判定: 未解決（中から見える cgroup は C1、区別は C1・C3、主の process が殺されたときは C2 で決まる）。偽れなさは設計の条件つき。

- `--cgroupns` の既定は cgroup v2 で private（[podman-run の --cgroupns](https://docs.podman.io/en/latest/markdown/podman-run.1.html#cgroupns-mode)）。
  中の `/sys/fs/cgroup` は container の cgroup を根として見せ、既定では読み取り専用（[podman-run の --security-opt unmask](https://docs.podman.io/en/latest/markdown/podman-run.1.html#security-opt-option)）。
- crun は systemd の cgroup manager と cgroup v2 で、scope の下に子の cgroup `container` を作って process を入れる（[crun.1.md L494-L512](https://github.com/containers/crun/blob/079ff6a7a16d029460af882f9ea44674e6a510b6/crun.1.md#L494-L512)、[cgroup-systemd.c L2069-L2083](https://github.com/containers/crun/blob/079ff6a7a16d029460af882f9ea44674e6a510b6/src/libcrun/cgroup-systemd.c#L2069-L2083)）。
  上限は scope に systemd の `MemoryMax` で付け（[cgroup-systemd.c L1577](https://github.com/containers/crun/blob/079ff6a7a16d029460af882f9ea44674e6a510b6/src/libcrun/cgroup-systemd.c#L1577)）、子の cgroup にも同じ resources を書く（[cgroup.c L393-L408](https://github.com/containers/crun/blob/079ff6a7a16d029460af882f9ea44674e6a510b6/src/libcrun/cgroup.c#L393-L408)）と読める。
- kernel の event は上限に達した memcg から根の手前まで祖先に向かって数え、子孫には数えない（[memcontrol.h L990-L1013](https://github.com/torvalds/linux/blob/e5f0a698b34ed76002dc5cff3804a61c80233a7a/include/linux/memcontrol.h#L990-L1013)）。
  そのため中の根（cgroup namespace の根）が上限を持つ cgroup か、その下かで、上限による OOM の `oom` が中から見えるかが変わる。
  中の根が `container` で、そこにも上限が書かれていれば見える。
  上限が scope にだけあれば、中の `oom` は増えず `oom_kill` だけが増え、machine 全体の OOM と同じに見える。
  どちらかはコードの読みだけでは確かでなく、実物で確かめる（C1 の `/proc/self/cgroup`・中の `memory.max`・中の `oom` の差分）。
- 主の process 自身が殺されたとき（C2）は後の値を読めず、何も返せない。
- 偽れなさ: 検証のコマンドは主の process と同じ uid で同じ container に居る。
  主の process が書ける返し道（stdout・mount した dir のファイル）は、コマンドの残りの process も書ける。
  同じ uid の process は、主の process が `PR_SET_DUMPABLE` を 0 にしていないと ptrace で触れうる（container は `--cap-drop all` で `CAP_SYS_PTRACE` を持たない）。
  偽れない形にするには、主の process が pid 1 として後の値を読む前に残りの process を全て殺し、non-dumpable で動き、制御側が最後の 1 つの報告だけを採る、という runtime の設計が要る。
  この条件は実物の C1〜C5 では確かめられず、採るなら後続の task の設計と review が持つ。

### (M3) 制御側が container の動いている間に machine の中の container の cgroup の memory.events を読む

判定: 不成立。

- 子 process の kill は container の終わりの直前にも起きうるので、事実を取りこぼさないには終わった後の値が要る。
- container の cgroup は終わりの後に消える。
  `--rm` では container ごと消え（[podman-run の --rm](https://docs.podman.io/en/latest/markdown/podman-run.1.html#rm)）、conmon 自身も終わりの時点で cgroup が消えていることを想定している（[cgroup.c L296-L303](https://github.com/containers/conmon/blob/c8cc2c4db27531bd4e084ce7857f73cd21ee639d/src/cgroup.c#L296-L303)）。
  動いている間の読みは終わりの値を保証せず、消えるのとの競合を避けられない。
- 終わった後も残る cgroup の値を読む形は (M4) にした。

### (M4) container を run ごとの親の slice の下に置き、制御側が slice の memory.events を前後で読む（調べで見つけた方法）

判定: 未解決（slice が作れ memory の controller が効くか、区別できるかは script の parent の C1・C3・C5 で決まる）。

- kernel の event は祖先へ数えるので（cgroup2 を `memory_localevents` で mount していないとき。[memcontrol.h L1009-L1010](https://github.com/torvalds/linux/blob/e5f0a698b34ed76002dc5cff3804a61c80233a7a/include/linux/memcontrol.h#L1009-L1010)）、container の cgroup の `oom`・`oom_kill` は親の slice の `memory.events` にも数えられ、container の cgroup が消えた後も slice が在る限り残る（[memcontrol.h L1000-L1012](https://github.com/torvalds/linux/blob/e5f0a698b34ed76002dc5cff3804a61c80233a7a/include/linux/memcontrol.h#L1000-L1012)）。
- `oom` を数えるのは memcg の上限での OOM（[memcontrol.c L1660-L1667](https://github.com/torvalds/linux/blob/e5f0a698b34ed76002dc5cff3804a61c80233a7a/mm/memcontrol.c#L1660-L1667)）と `memory.max` を下げたときの OOM（[L4443](https://github.com/torvalds/linux/blob/e5f0a698b34ed76002dc5cff3804a61c80233a7a/mm/memcontrol.c#L4443)）だけ。
  `oom_kill` はどの OOM killer の kill でも殺された process の memcg から数える（[oom_kill.c L944-L945](https://github.com/torvalds/linux/blob/e5f0a698b34ed76002dc5cff3804a61c80233a7a/mm/oom_kill.c#L944-L945)、[cgroup-v2 の memory.events](https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html#memory-interface-files)）。
  そのため slice の `oom` の差分が 0 で `oom_kill` の差分が正なら、上限の外（machine 全体か、slice より上の祖先の上限）による kill と読める。
  `oom` が正なら slice か その下の上限での OOM が起きたことで、同時に machine 全体の OOM も起きた場合は分けきれない（その組は「原因の候補」に両方を書く）。
- 偽れなさ: slice は container の cgroup namespace の外で、中からは見えず書けない（kernel の値で、主の process を通らない）。
- 費用と汎用性: slice が消えないよう制御側が持つもの（script では slice の中で眠る unit）が要り、終わりに片付ける。
  macOS では machine の中の cgroupfs を読む道（`podman machine ssh`）が要り、Linux の host では直に読める。
  言語やツールの出力は見ず kernel の interface だけを読むが、cgroup v2・systemd の cgroup manager・`--cgroup-parent` の扱いに依るので、runtime に固定せず設定で受ける形になる（[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)）。
- rootless の Podman が run ごとの slice の `--cgroup-parent` を受け、slice の `memory.events` が在るかは資料で決まらず、実物で確かめる。

### (M5) podman events の died の OOMKilled を、--rm のまま container の名前と時刻で読む（調べで見つけた方法）

判定: OOM の事態の記録としては未解決（取りこぼさないかは C1・C4 で決まる）、kill の断定と、上限と machine 全体の区別は不成立。

- (1) のとおり died の event は削除の前に書かれ、`OOMKilled` を持つ。
  `podman events --since <起動の時刻> --filter container=<名前> --filter event=died --format '{{.OOMKilled}}'` で `--rm` のまま読める。
- 中身は M1 と同じ conmon の `oom` のファイルなので、`oom` だけの増え（kill なし）と kill を区別せず、上限と machine 全体も区別しない。
- 子 process だけの kill は、`oom_kill` が cgroup の全ての process を数えるので、conmon が終わりの時点で cgroup を読めれば true として載る。
  読めるか（cgroup の dir が残っているか）は C1・C4 で確かめる。
  ただし true からは kill だったとは言えない（(1) の `oom` の項）。
- 偽れなさ: conmon と libpod が container の外で書くので、中のコマンドは偽の true を作れない。
- `events_logger` が `none` なら event が無いので、無いことを「OOM なし」と読まず「読めない」とする。

### (M6) machine の kernel のログの OOM の行を読む

判定: 不成立（runtime の方法としては採らない）。

- kernel は memcg の上限の kill に「Memory cgroup out of memory: Killed process」、machine 全体の kill に「Out of memory: Killed process」を出すが、文の形は interface として約束されていない。
  machine の全ての container の行が混ざり、run との対応は文の中の cgroup の path を読むことになる。
- 実物の確かめでは、他の値の答え合わせの列（dmesg の OOM の行）としてだけ使う。

## (3) 方法の比べ

| 方法 | 子 process だけの kill | 上限と machine 全体の区別 | 取りこぼし | 偽れなさ | runtime の汎用性 |
| --- | --- | --- | --- | --- | --- |
| M1 inspect | OOM の事態として載る見込み（C1）、kill かは言えない（`oom` だけの増えでも true） | 不成立（conmon が区別しない） | watch は外れるが終わりの読みが拾う見込み、cgroup が先に消えれば落ちる（C1・C4） | 成立（外で書く） | 成立。ただし全ての経路の rm と名前の再利用の費用 |
| M2 中から読む | 中の根の見え方による（C1） | 中の根に上限があれば成立（C1・C3） | 主の process が殺されると読めない（C2） | 設計の条件つき（non-dumpable・残りを殺してから報告） | 成立（kernel の interface） |
| M3 動いている間に外から | 終わりの値を保証しない | 値は分けられるが読めない | 不成立（--rm と後片付けとの競合） | 成立 | — |
| M4 親の slice | 成立の見込み（祖先へ数える） | 成立の見込み（`oom` の差分の有無、C1・C3） | 無い見込み（slice が残る限り値が残る） | 成立（中から見えない） | 設定で受ければ成立（cgroup v2・systemd・machine への道） |
| M5 died の event | M1 と同じ（C1）、kill かは言えない | 不成立 | M1 と同じ（C1・C4） | 成立 | 成立（`events_logger` が none なら読めない） |
| M6 kernel のログ | 載る | 文で分かる | ring buffer があふれれば落ちる | 成立 | 不成立（約束の無い文を読む） |

M2・M4 は `oom_kill` の増えで、M6 は kernel のログの kill の行で「OOM killer が中の process を殺した」ことを言え、M1・M5 は「container の cgroup で OOM の事態が起きた」ことまでしか言わない。
どの方法も、その kill や事態が検証の失敗の原因だったことは言わない。
検証のコマンドはわざとメモリを使って本物の OOM を起こせるので、OOM の事実を理由に失敗を成功に変えてはならない（fail closed の向き）。
OOM の事実で変えてよいのは、失敗の扱い（やり直し・控え・attention の文）までで、その回数に上限を付ける。

## 全体の判定

未解決。
(1) はコードで決まり、M3 と M6 は不成立で、M1・M5 の true は kill を言わない（kill には `oom_kill` の根拠が要る）。
OOM の事態の記録（M1・M5）が子 process の kill を取りこぼさないかは C1・C4、上限と machine 全体の区別（M2・M4）は C1・C3（M4 は script の parent）、主の process が殺されたときの扱いは C2、偽の OOM が出ないことは C5 の実物の結果で決まる。

## 実物での確かめの手順

- script は [container-oom-detection/oom-cases.sh](container-oom-detection/oom-cases.sh)。
  使い方は冒頭の comment にある。
  podman のコマンドには全て `--connection dagq-spike` を付け、machine は [container-runtime-spike/machine.sh](container-runtime-spike/machine.sh) の `dagq-spike` を使う（人の既定の接続を変えない）。
- 守ることは container-runtime-spike.md の「共通の前提」と同じ（人の既定の machine と接続を変えない、本番と使い捨ての queue を動かさない、queue に触るのは結果の note だけ）。
  C3 は machine 全体の OOM を起こすので、`dagq-spike` は Spike 専用で、他の machine を動かしていないこと（`machine.sh` が確かめる）。
- container は hardened と同じ制限（`--memory`・`--cpus`・`--pids-limit`・`--read-only`・`--tmpfs /tmp`・`--userns keep-id`・`--user`・`--cap-drop all`・`no-new-privileges`・`--network none`）で、image は `docker.io/library/debian:12.7-slim`（dagq の image は build しない）。
  主の process は `dagq relay verify` の代わりの `sh` で、子の前後に中の `/sys/fs/cgroup` を読み、mount した `WORKDIR/oom/<id>/` に書く。
  メモリは言語に依らず、`head -c` で `/dev/zero` を `tail` に流して使う（改行の無い 1 行を `tail` がメモリに持つ）。

```sh
REPO=<main checkout の path>
S=$REPO/docs/plans/container-runtime-spike
O=$REPO/docs/plans/container-oom-detection
SPIKE=$(mktemp -d "$TMPDIR/dagq-oom.XXXXXX")
sh "$S/machine.sh" up "$SPIKE" 2 2048
sh "$O/oom-cases.sh" matrix "$SPIKE" 512
sh "$O/oom-cases.sh" parent "$SPIKE" 512
sh "$S/machine.sh" down
```

### 場合

| 場合 | 中で起きること | 確かめること |
| --- | --- | --- |
| C5 | 子が OOM なしで exit 101 を返すだけ | OOM の無い失敗で全ての方法が何も出さないこと。`oom` だけが増えて kill が無い形の偽の陽性は C5 では除けない（下の「保証できない点」） |
| C1 | 子の `tail` が container の上限を超えて殺され、子の `sh` は exit 101 で終わる（cargo が rustc の死を自分の失敗で返す形） | 主の exit code に跡が無いこと、State.OOMKilled・died の欄・中と親の `oom` と `oom_kill` の差分、中の根（`/proc/self/cgroup`）と中の `memory.max` |
| C2 | 主の process が `exec tail` で上限を超えて殺される | 主の後の値が無いこと、podman run の exit code（137 の見込み）と State.OOMKilled・died の欄 |
| C4 | 上限の 2 倍の page cache を書いて `max` を増やし、制御側が conmon の watch が外れたのを確かめてから、子が上限を超える | watch が OOM の前に外れた形で、終わりの読みだけで State.OOMKilled・died の欄が true になるか。C1 でも `max` は OOM の前に増えるが、watch が外れたかは分からないので、C4 がその形を確かめて作る（下の「C4 の試行が有効な条件」） |
| C3 | 上限を machine のメモリの 2 倍にし、子が machine のメモリを使い尽くす | 中と親の `oom` の差分が 0 で `oom_kill` だけが増えるか、dmesg の行が machine 全体の OOM か、State.OOMKilled・died の欄 |

- 周回: 場合と `--rm` の有無の組ごとに 2 回、交互にしない。
  各組は有無を見る観察で値の平均を取らず、2 回目は再現の確かめ。
- 順: C5→C1→C2→C4→C3（machine が不安定になりうる C3 を最後に）。
  各場合の中は `--rm` ありの 2 回、なしの 2 回の順。
- parent（M4）は C5・C1・C3 を `--rm` ありで 2 回ずつ、container ごとに専用の slice（眠る unit で保つ）の下で流し、slice の `oom` と `oom_kill` を前後で読む。
- 値の計算は差分（後−前）だけで、script が「前>後（差分）」と書く。
  C4 の `max` の列は「前>page cache を書いた後」。

### C4 の試行が有効な条件

script は C4 を段に分け、中の主の process と制御側が run の dir のファイルで順を合わせる。
中は `before` を書いた後に制御側の合図を待ち、page cache を書いて `mid` を書き、もう一度合図を待ってから子を走らせる。
制御側は、最初の合図の前と、`mid` の 2 秒後（2 度目の合図の前）に、conmon（`podman inspect` の `State.ConmonPid`）が持つ inotify の fd の数を machine の `/proc/<pid>/fd` で数える。
conmon は OOM のない通知で watch を外すとき、その fd を閉じる（[cgroup.c L203-L207](https://github.com/containers/conmon/blob/c8cc2c4db27531bd4e084ce7857f73cd21ee639d/src/cgroup.c#L203-L207)）。

- 試行を有効とするのは、次の全てを満たすときだけ。
  - 中と制御側がそれぞれ相手のファイル（中は 2 つの合図、制御側は `before` と `mid` の後の印）を時間内に見た。
  - page cache を書く `dd` と、その後のファイルの削除が成功した。
  - 中の `max` が `before` より `mid` で増えた。
  - 中の `oom` と `oom_kill` が `before` と `mid` で同じ（子を走らせる前に OOM が起きていない）。
  - conmon の inotify の fd が、最初の合図の前に 1 つ以上あり、`mid` の後に 0 になった。
    conmon の process が見つからない（pid が取れない・0・居ない）ときは数を `-` とし、watch が外れたとは読まない。
- どれかを満たさなければ、満たさないと気づいた側（中か制御側）が最初の理由を `invalid` のファイルに書き、中は子を走らせない。
  制御側は inotify の条件を 2 度目の合図の前に確かめるので、満たさない試行で子が走ることは無い。
  表の「C4 の前提」の列は「有効」か「無効: 理由」と、inotify の fd の数の前>`mid` の後を出す。
- 2 回とも無効なら、watch が外れた後の取りこぼしの問いは未解決のまま note に書き、C1 の結果だけで M1・M5 を判定しない。
- 保証できない点: inotify の fd の数は、conmon が他に inotify を開かないことを前提に watch の有無と読む（conmon で `inotify_init` を呼ぶのは OOM の watch の [cgroup.c L121](https://github.com/containers/conmon/blob/c8cc2c4db27531bd4e084ce7857f73cd21ee639d/src/cgroup.c#L121) だけと読める）。
  前提が崩れれば `mid` の後も 0 にならず、試行は無効の側に倒れる。
  有効な試行でも、fd を閉じたことから読めるのは watch が外れたことまでで、conmon の内部の状態をそれ以上は見ない。
  page cache が machine の mount で memcg に数えられなければ `max` が増えず、試行は無効になる。

### 保証できない点

- `oom` だけが増えて誰も殺されない形（(1) の `oom` の項）は、中の権限の無い process からは狙って作れないので、場合として作らない。
- 代わりに表の「OOMKilled と oom_kill の食い違い」の列が、両方の向きの食い違いを出す。
  State.OOMKilled か died の欄が true なのに中の `oom_kill` が増えていない行が 1 つでもあれば、M1・M5 の true を kill と読めないことの実物の例になる。
  無くても、偽の陽性が無いことの証にはならない。
  中の `oom_kill` が増えたのに true が無い行は、M1・M5 の取りこぼしの例になる。

### 表の列

場合・`--rm`・回・`memory.events` の `oom` と `oom_kill` の前後（中から）・`memory.events.local` の `oom` と `oom_kill` の前後・`max` の前後（C4）・`memory.max`（中から）・主の exit code・子の exit code（`tail` の。殺されれば 137）・podman run の exit code・State.OOMKilled/ExitCode（`--rm` なしだけ）・died の欄の OOMKilled/exit code・machine の dmesg の OOM の行の増え（memcg と machine 全体）・`/proc/self/cgroup`・C4 の前提（conmon の inotify の fd の数の前>`mid` の後と、無効の理由）・OOMKilled と `oom_kill` の食い違い。
parent では、これに親の slice の `oom` と `oom_kill` の前後を足す。
script は表を `WORKDIR/oom/table.md` に足し、頭の comment に Podman・OCI runtime・cgroup の版と manager・kernel・`ptrace_scope`・machine のメモリを残す。

## 結果を goal 184 の note に残す書式

matrix と parent をまとめて 1 つの note にし、固定バイナリで `$REPO` の中で打つ。

```sh
cd "$REPO"
~/.local/bin/dagq note --goal 184 --kind spike --text "$(cat "$SPIKE/note-oom.md")"
```

note の本文の書式:

```text
container-oom-detection（OOM kill を事実として読めるか）
日付: 2026-10-DD（JST）
環境: macOS <版> <arch>・podman <版>・machine dagq-spike <CPU>/<メモリ>・table.md の頭の comment の行
表:
<WORKDIR/oom/table.md の matrix と parent の行をすべて>
判定:
- OOM の事態の記録（M1・M5）: C1 と有効な C4 の State.OOMKilled と died の欄が 2 回とも true / どれかが false（どの組か）
- C4 の有効な試行: 何回か（無効の理由）
- OOMKilled と oom_kill の食い違い: 無い / 在る（どの行か）
- 中から（M2）: C1 の中の oom の差分が正 / 0、C3 の中の oom の差分が 0 / 正、C2 で後の値が無い
- 親の slice（M4）: slice を作れた / 作れない、C1 の oom が正・C3 の oom が 0 で oom_kill が正 / それ以外
- 偽の陽性: C5 で全て 0 と false / どれかが出た
全体: 成立 / 不成立 / 一部（下の「結果ごとの後続の task の形」のどれに当たるか）
手順: docs/plans/container-oom-detection.md
```

この文書の表に写して `status` を `completed` にするのと、結果で後続の task を再計画するのは、note を読む planner が決める。

## 結果ごとの後続の task の形

実物の結果（上の note）を待つのは、OOM kill を verification の失敗と取り違えない task（task 3162。以下「OOM の task」）だけで、結果が出るまで draft に置く。
同時に動く container の数の抑え方（以下「上限の task」の列）は ADR-t3163-1 が決めた予約で、実物の結果を待たない。
(B) と、verification だけ予約して turn は重ねる (C) は今の材料では採らず、実物の結果は同 ADR の「重ねる形を見直す条件」の 1 つ目（M4 か偽れなさの条件を持てる M2 の成立）に当たるかだけを決める。
見直しは同 ADR の amends で、実物の結果を読む planner が別の task として足し、他の 2 つの条件（重なる見込みが低いと測れること・予約した側を守る手）も揃うときに限る。

| 実物の結果 | OOM の task の形 | 上限の task（ADR-t3163-1 の予約との関係） |
| --- | --- | --- |
| M4 が成立（slice を作れ、C1 で slice の `oom` が正、C3 で `oom` が 0 で `oom_kill` が正、C5 で 0） | M4 で再計画する。制御側は run ごとの slice を持って前後の差分を読み、M5 を答え合わせに使う。slice と machine への道は設定で受ける | 予約のまま。見直しの条件の 1 つ目に当たるので、他の 2 つが揃えば (B) か (C) を amends で見直せる |
| M4 は不成立で、M2 の中の値が区別できる（C1 で中の `oom` が正、C3 で 0） | M2 で再計画できるが、主の process の偽れなさの条件（non-dumpable・残りを殺してから報告・最後の報告だけを採る）を設計に入れ、C2 は M5 の記録（OOM の事態が起きたことまで）だけに落とす。条件を持てなければ下の行にする | 予約のまま。M2 の条件を持てたときだけ見直しの条件の 1 つ目に当たり、他の 2 つが揃えば (B) か (C) を amends で見直せる |
| 区別はできないが M5 が OOM の事態を取りこぼさない（C1・C2・有効な C4 で died の欄が true、C5 で無い） | attention の文を「事実」と「原因の候補」に分ける形に縮める。事実は「container の cgroup で OOM の事態が起きた（podman の died の OOMKilled。process が殺されたとは限らない）」、原因の候補は「container の上限（`--memory`）か、machine 全体のメモリ不足」。kill と書くのは `oom_kill` の増えを読める方法（M2・M4）を併せたときだけ。失敗を成功や控えに変えない | 予約のまま。見直しの条件に当たらないので重ねる形を見直さない |
| M5 も取りこぼす（C1・C2・有効な C4 のどれかで died の欄が無い）か、どの方法でも取れない | OOM を事実として言えないので中止するか、attention に「OOM かどうかは確かめられない」とだけ書く形にする | 予約のまま。見直しの条件に当たらないので重ねる形を見直さない |

- どの行でも、OOM の事実で失敗を成功に変えない（上の「(3) 方法の比べ」の末尾）。
- M1・M5 の true だけで「殺された」と書かない。kill の断定には `oom_kill` の増え（M2・M4）が要る。
- 行に当てはまらない組（2 回の結果が食い違うなど）は、planner が note の表から決め、決めきれなければ人に上げる。
