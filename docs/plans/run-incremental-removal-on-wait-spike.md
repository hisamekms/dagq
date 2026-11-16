---
id: plan-run-incremental-removal-on-wait-spike
type: plan
title: スパイク：待ちの run の target を消す今の仕組みの代償（消した後の作り直しと turn の時間切れ）と、incremental だけ・target 全部を消したときの手直しの最初の build の延び、待ちの種類と経過ごとの消す条件
status: completed
created: 2026-11-17
owners:
  - hisamekms
tags:
  - planning
  - performance
  - measurement
related:
  - plan-run-build-output-size-spike
  - plan-incremental-session-removal-spike
  - design-supervisor-lifecycle-run-worktrees
  - design-supervisor-lifecycle-waiting
  - development-local-checks
---

# スパイク：待ちの run の target を消す条件

goal 286 の案 B を「今の消し方の代償を測り、消す条件を決める」に書き直した Spike。
問いは Q0（今の `build_outputs_removed` の 1 件ごとの代償、記録だけ）、Q1（build の止まった run の target で、incremental だけ消す I と target 全部を消す A が、消さない K に比べて手直しの最初の build を何秒延ばし何 GiB 空けるか）、Q2（待ちの種類 r と経過 e ごとの消す条件、disk_space の消し方の除外、次の turn の上限の扱い）。
前の測定は [run の target の内訳の Spike](run-build-output-size-spike.md) と [incremental の古い session の Spike](incremental-session-removal-spike.md)。

判定は Q0 が「消した後の turn は長いが、上限で止まる割合は高くない」、Q1 は I が **unresolved**・A が **does_not_hold**、Q2 は種類ごとの e_r(A) が出て disk_space の除外の条件（C2）も決まり、問い全体は **holds**（下の「判定と根拠」）。

要点：

- 観測区間（2026-11-02T18:02:38Z〜2026-11-16T18:02:38Z）の削除は 443 件（disk_space 251・awaiting_answer 171・run_ended 21）で、削除のある run は 220 本。待ちの区間と turn は、削除か着地のある 374 本の run から作った。
- `build_outputs_removed` の時刻は、掃除の job が終わってから記録される時刻で、実際に消した時刻ではない。実際の削除は、記録の時刻までに始まっていた待ちの区間（ask の付いた削除はその ask の区間、ask の無い削除は ask の無い区間）の中で、記録の時刻以前のどこかとして扱い、e・残りの待ち・保留などを範囲で出した。範囲で決まらない値は不明とした。
- 消した後の最初の revise・resume の turn（221 件）は、消していない revise・resume の turn（452 件）より中央値で 851 秒長い（未終了の 1 件を両端に置くと 854 秒、resume どうしでは 1432 秒と 758 秒で 674 秒）。上限で止まった割合は 1.4% と 1.3%（resume どうしでは 1.4% と 5.5%）で、差は 10 ポイントに遠い。
- 「消す → 作り直しの turn が上限で止まる → また消す」の悪循環は 3 件（task 4175・4133・5011）、決まらない行が 3 件。
- Q1：K の T の中央値 134.5 秒に対し、I は 474 秒（Δt = 339.5 秒、対ごとの比 2.83〜4.22）、A は 443 秒（Δt = 308.5 秒）。空く量は I が 9.04 GiB、A が 12.42 GiB（G_run = 12.36 GiB）。A は I より空く量が多く、延びは小さい。
- Q2（待ちの続きを phase の変わり目と ask の作成・答え・close の時刻で切り、同じ種類の続きをまとめた 1550 区間。起点は ask の区間が ask の作成、ask の無い区間が直前の turn_finished）：M_r(e, A) で、着地 slot の待ち・approve_landing・worker_question・needs_session（上限の停止でない resume の前）・ask の無い landing_answer と waiting・returning・上限で止まった後の resume の待ち・held は e_r(A) = 0、stalled は「消さない」、review_held・blocked は標本不足、e2e の待ち（90 区間は 3 本の run の断片）は標本不足で決まらない。
- 起点を各区間の始まり（その種類の待ちに入った時刻）に置き換えた参考の値では、ask の無い landing_answer・waiting と returning が 5 分、上限で止まった後の resume の待ちが「消さない」に変わる。ask の無い区間は、直前の turn_finished から区間の始まりまでに検証・review・前の答えの待ちを挟むので、起点の取り方で e_r が変わる（判定は task の指定の起点で出した）。
- disk_space の消し方の除外は、description の選び方（保留の解けた件を全部残す条件のうち、除く対象を最も多く除くもの）で **C2（turn の上限で止まった後の resume の待ちを除く）**。不明の件を両端に置いた 2 通りとも C2 で、除くのは 4 件（除く対象 3 件）。
- disk_space の 251 件のうち除く対象は 138〜155 件（不明 17 件を両端に置いた値）で、そのうち 111〜121 件は着地 slot の待ち（多くは消してから 10 分以内に着地が始まった）。消す時点の種類と経過では見分けられない。

## 環境

| 項目 | 値 |
|---|---|
| commit | この run の base と測った tree は `2fd10132ff9ea0685b0a2f4244bc2e08d61e4bd2`（branch `dagq/5985e18c-75bc-4fa2-bd07-bec6ce95f55e` の起点）。この文書の commit は receipt に書く |
| rustc / cargo | 1.99.1（58cf8f9c3 2026-10-13）/ 1.99.1（5f94df478 2026-08-27）（worktree の `rust-toolchain.toml`） |
| sccache | 0.19.0（run の dagq-rustc-wrapper 経由。server は host の全 run が共有） |
| host | Apple M1・8 コア・16 GiB、macOS 27.2.1（26B131）。APFS の空きは測定の間に 26〜31 GiB |
| env | worker の `[run.env]` のまま（`CARGO_BUILD_JOBS=4`・`RUST_TEST_THREADS=6`・`NEXTEST_TEST_THREADS=6`・`RUSTC_WRAPPER` は run の wrapper）。script は run の TMPDIR に置いた |
| provider | Claude の非対話の worker（この run）。build と測定は全てこの turn の中、自分の worktree の target |
| 同時の load | 本番の他の run が同時に build していた。測定の 10 回の始めと終わりの 1 分値は 3.32〜8.97（16 を超えた回は無い）。開始の状態を作る間に 41 まで上がった時があった |
| 読み取りの時刻 t₀ | 2026-11-16T18:02:38Z（`dagq events` はすべて `--until` t₀） |

## 打ったもの

script と生の CSV・cargo の出力・events の dump は run の TMPDIR に置き、commit していない。本文は下に引く。

### patch E1・E2

どちらも `src/domain/disk.rs` の関数の本体に 1 行足す（file を丸ごと置き換える形で当て、終わった後に `git checkout -- src/domain/disk.rs` で戻した）。

```diff
--- a/src/domain/disk.rs
+++ b/src/domain/disk.rs
@@ -322,6 +322,7 @@
 /// The free bytes no claim nor landing goes below: the larger of
 /// [`MIN_RESERVE_BYTES`] and `share` of `total`, the size of the volume.
 pub fn reserve_bytes(total: u64, share: f64) -> u64 {
+    let _probe = "e1";
     MIN_RESERVE_BYTES.max((total as f64 * share).ceil() as u64)
 }
 
```

```diff
--- a/src/domain/disk.rs
+++ b/src/domain/disk.rs
@@ -691,6 +691,7 @@
 
 /// `bytes` in GiB with one decimal, for messages.
 pub fn gib(bytes: f64) -> String {
+    let _probe = "e2";
     format!("{:.1} GiB", bytes / f64::from(1u32 << 30))
 }
 
```

### 開始の状態 S の作り方（setup.sh）

```sh
#!/bin/sh
# setup.sh: base の tree で手元の検証の 3 本、E1 を当ててもう一度。秒を setup.csv に
set -u
SP=$TMPDIR/sp; W=$(cat $SP/worktree); cd "$W"
one() { s=$(date +%s); "$@" >>"$SP/setup.log" 2>&1; rc=$?; echo "$tag,$*,$(( $(date +%s) - s )),$rc" | tee -a "$SP/setup.csv"; }
tag=base
one cargo clippy --locked --all-targets -- -D warnings
one cargo test --locked --lib
one cargo test --locked --test it --no-run
cp "$SP/disk.e1" src/domain/disk.rs
tag=e1
one cargo clippy --locked --all-targets -- -D warnings
one cargo test --locked --lib
one cargo test --locked --test it --no-run
du -sk target target/debug/incremental | tee -a "$SP/setup.csv"
echo SETUP_DONE
```

- 時間：base で clippy 222 秒、`cargo test --locked --lib` の build 11 分 57 秒（Finished の値）、`--test it --no-run` 68 秒。E1 の後に clippy 76 秒、lib の build 71 秒、it 11 秒。
- `cargo test --locked --lib` は test binary の build の後に test の実行に入り、base で 20 分を超えても終わらなかった（他の run の負荷の下で 1890 件通った後に遅くなった）。build の出力は test の実行で変わらないので、test の process（自分が起動した pid）を止めた。E1 の後も build の Finished の後に test の process を止めた。setup.csv の終了の status 101 はこの停止による。
- S の後の target は `du -sk` で 13,191,920 KiB（12.58 GiB）、incremental は 11,279,236 KiB（10.76 GiB）。

### clone.py と写し取りの確認の script（check.py）

clone.py は [incremental の古い session の Spike](incremental-session-removal-spike.md) の「1 回を流す script」の本文をそのまま TMPDIR に写し、`python3 -I` で使った。

```python
# clone.py SRC DST: SRC の木を APFS の clonefile で写し、hard link を保つ（cp -cRp は hard link を切る）
import ctypes, os, sys
libc = ctypes.CDLL("libc.dylib", use_errno=True)
src, dst = sys.argv[1], sys.argv[2]
seen = {}
def clone(s, d):
    if libc.clonefile(s.encode(), d.encode(), 0x0001) != 0:  # CLONE_NOFOLLOW
        e = ctypes.get_errno(); raise OSError(e, os.strerror(e), s)
os.mkdir(dst)
for root, dirs, files in os.walk(src):
    rel = os.path.relpath(root, src)
    droot = os.path.join(dst, rel) if rel != "." else dst
    for name in dirs:
        p = os.path.join(root, name); q = os.path.join(droot, name)
        if os.path.islink(p): clone(p, q)
        else: os.mkdir(q)
    for name in files:
        p = os.path.join(root, name); q = os.path.join(droot, name)
        st = os.lstat(p)
        key = (st.st_dev, st.st_ino)
        if st.st_nlink > 1 and key in seen: os.link(seen[key], q)
        else:
            clone(p, q); seen[key] = q
# 時刻を元に合わせる（dir は中身を作った後に）
for root, dirs, files in os.walk(src, topdown=False):
    rel = os.path.relpath(root, src)
    droot = os.path.join(dst, rel) if rel != "." else dst
    for name in files + dirs:
        st = os.lstat(os.path.join(root, name))
        os.utime(os.path.join(droot, name), ns=(st.st_atime_ns, st.st_mtime_ns), follow_symlinks=False)
    st = os.stat(root); os.utime(droot, ns=(st.st_atime_ns, st.st_mtime_ns))
```

```python
# check.py A B: target A と B の hard link の組（incremental の中の st_nlink > 1 の file を inode ごとにまとめた相対 path の組）
# の集合と、target・incremental の du -sk を比べる。出力: 組の数A 組の数B 一致 duA duB incA incB 判定
import os, sys, subprocess
def groups(t):
    inc = os.path.join(t, "debug", "incremental"); g = {}
    for root, dirs, files in os.walk(inc):
        for n in files:
            p = os.path.join(root, n); st = os.lstat(p)
            if st.st_nlink > 1:
                g.setdefault((st.st_dev, st.st_ino), []).append(os.path.relpath(p, inc))
    return {tuple(sorted(v)) for v in g.values()}
def du(p): return int(subprocess.run(["du", "-sk", p], capture_output=True, text=True).stdout.split()[0])
a, b = sys.argv[1], sys.argv[2]
ga, gb = groups(a), groups(b)
da, db = du(a), du(b)
ia, ib = du(os.path.join(a, "debug/incremental")), du(os.path.join(b, "debug/incremental"))
ok = ga == gb and abs(da - db) <= 0.02 * max(da, db) and abs(ia - ib) <= 0.02 * max(ia, ib)
print(len(ga), len(gb), ga == gb, da, db, ia, ib, "OK" if ok else "NG")
sys.exit(0 if ok else 1)
```

### 1 回を流す script（run.sh）と並び（batch.sh）

```sh
#!/bin/sh
# run.sh COND(K|I|A) IDX : S から戻し、E2 を当て、COND に従って消し、3 本を流して results.csv に 1 行足す
set -u
COND=$1 IDX=$2
SP=$TMPDIR/sp; W=$(cat "$SP/worktree"); cd "$W"
LOG=$SP/log-$IDX-$COND
gk() { du -sk "$1" 2>/dev/null | awk '{print $1+0}'; }
# 戻しと写し取りの確認（一致しなければ 1 回だけ戻し直す）
chk=NG; tries=0
while [ "$chk" != OK ] && [ $tries -lt 2 ]; do
  tries=$((tries+1))
  rm -rf target && python3 -I "$SP/clone.py" "$SP/S" target
  c=$(python3 -I "$SP/check.py" target "$SP/S"); chk=$(echo "$c" | awk '{print $NF}')
  echo "$IDX,$COND,try$tries,$c" >> "$SP/restore.csv"
done
[ "$chk" = OK ] || { echo "$IDX,$COND,restore_failed" >> "$SP/results.csv"; exit 1; }
cp "$SP/disk.e1" src/domain/disk.rs && cp "$SP/disk.e1e2" src/domain/disk.rs
b=$(gk target)
case $COND in I) rm -rf target/debug/incremental ;; A) rm -rf target ;; esac
a=$(gk target); [ -z "$a" ] && a=0
removed=$((b - a))
load0=$(sysctl -n vm.loadavg | awk '{print $2}')
stats() { sccache --show-stats --stats-format json 2>/dev/null | python3 -I -c 'import json,sys;d=json.load(sys.stdin)["stats"];print(d["cache_hits"]["counts"].get("Rust",0),d["cache_misses"]["counts"].get("Rust",0))'; }
set -- $(stats); h0=$1 m0=$2
start=$(date +%s); out=""; fin=""
for c in lib it clippy; do
  left=$((1800 - ($(date +%s) - start))); [ $left -lt 1 ] && left=1
  case $c in
    lib) set -- cargo test --locked --lib --no-run ;;
    it) set -- cargo test --locked --test it --no-run ;;
    clippy) set -- cargo clippy --locked --all-targets -- -D warnings ;;
  esac
  s=$(date +%s); timeout $left "$@" > "$LOG-$c" 2>&1; rc=$?; e=$(( $(date +%s) - s ))
  f=$(grep -o 'Finished .* in [0-9hms. ]*' "$LOG-$c" | tail -1 | sed 's/.* in //')
  out="$out,$e,$rc"; fin="$fin;$c=$f"
done
T=$(( $(date +%s) - start ))
set -- $(stats); dh=$(( $1 - h0 )) dm=$(( $2 - m0 ))
load1=$(sysctl -n vm.loadavg | awk '{print $2}')
after=$(gk target)
comp=$(cat "$LOG-lib" "$LOG-it" "$LOG-clippy" | grep -cE '^ *(Compiling|Checking) ')
g() { echo "$1" | awk '{printf "%.2f", $1/1048576}'; }
echo "$IDX,$COND$out,$T,$(g $b),$(g $removed),$(g $after),$dh/$dm,$load0,$load1,$comp,$fin,$(date -u +%H:%M:%S)" | tee -a "$SP/results.csv"
```

```sh
#!/bin/sh
SP=$TMPDIR/sp
for x in 1:K 2:I 3:I 4:K 5:A 6:I 7:K 8:K 9:I 10:A; do
  sh "$SP/run.sh" "${x#*:}" "${x%%:*}"
  avail=$(df -k "$SP" | awk 'NR==2{print $4}'); echo "after ${x}: avail_kb=$avail" >> "$SP/batch.log"
  [ "$avail" -lt 20971520 ] && { echo "stop: free below 20 GiB" >> "$SP/batch.log"; exit 1; }
done
echo BATCH_DONE >> "$SP/batch.log"
```

### 記録の読み取り

読んだ events（すべて `--full`・`--until 2026-11-16T18:02:38Z`）：

- 種類ごと（`--since 2026-11-02T18:02:38Z`）：`build_outputs_removed`・`run_integrated`・`claim_held`・`claim_resumed`・`claim_deferred`・`claim_deferral_ended`・`incremental_sessions_removed`。
- run ごと（`--run ID --all`、run の始まりまで遡る）：上の `build_outputs_removed` か `run_integrated` がある 374 本。中の kind のうち使ったのは `run_phase_changed`（待ちの区間と種類）、`turn_started`・`turn_finished`・`exit_requested`（turn と上限の停止、e の起点）、`ask_opened`・`ask_answered`・`ask_closed`（ask の kind と開いている間、e の起点）、`build_outputs_removed`。`turn_requested`・`run_waiting_started`・`run_waiting_ended`・`resume_started`・`landing_queued` は確かめに読んだ。
- `dagq timeline RUN` は 4133 の run（ff0e1ccb）で gap の reason を確かめた（`landing_answer`・`waiting`・`needs_session`・`landing_queue`・`resume` など）。gap の reason は始まりの `run_phase_changed` の phase なので、全 run の集計は `run_phase_changed` から直接作った。
- 権限で拒まれた読み・読めない kind は無かった。queue の読み取りは最初の turn の約 3 分だけで、review の差し戻しの後の集計は同じ dump を読み直した。

#### 待ちの区間と種類（ana3.py）

- 待ちの phase：`landing_answer`・`waiting`・`needs_session`・`landing_queue`・`awaiting_slot`・`held`・`review_held`・`returning`・`awaiting_e2e`。この集合の外の phase（`worker`・`revise`・`resume`・`landing`・`validating`・`e2e`・`ended` など）までの続きを「待ちの続き」とする。
- 待ちの続きを、phase の変わり目と、ask の作成・答え・close の時刻（続きの中にあるもの）で小区間に切る。ask が開いているのは作成から、答えか close の早いほうまで。
- 小区間ごとに、その始まりに開いている ask があればその kind（2 つ以上なら早く作られたもの）で「answer:kind」、無ければ phase の名前で種類を付ける（下の表）。同じ種類で同じ ask の小区間の続きを 1 つの区間にまとめる。ask の作成の前の部分と答えの後の部分は ask の区間に入らない（例：着地の検証が止まって phase が `landing_answer` になってから approve_landing の ask が作られるまでは、ask の無い landing_answer の区間）。
- e と L の起点（description の指定のとおり）：ask の区間はその ask の作成。ask の無い区間は、区間の始まり以前で最後の `turn_finished`（run の始まりまで遡る）。どちらも無い区間は起点が記録に無いので、e と L を不明にした（観測区間の中に始まりがある 10 区間。下の「Q2」で影響を示す）。
- この起点では、ask の無い区間の e と L に、直前の turn の後の検証・review・前の答えの待ちの時間が入る。起点から区間の始まりまでの中央値は、終わった区間で着地 slot の待ちが 529 秒、needs_session_other が 2351 秒（未終了を含む 204 区間では 2369 秒）、ask の無い landing_answer が 2702 秒。Q2 には、起点を区間の始まりにした参考の値も添えた（判定には使わない）。
- 待ちの終わり（Q0 の「消してから待ちの終わり」と「10 分以内に待ちが終わった」）：description の「答え・resume・着地の始まり」に合わせ、ask の区間は答えか close、ask の無い区間は区間の始まりより後で最初の ask の答え・close と、待ちの続きの終わり（resume・着地などの始まり）の早いほうとした。ask の作成や、ask の無い種類どうしの変わり目では待ちは終わらない。Q2 の L は種類ごとの区間の長さ（区間の終わりまで）で、これとは別。
- 戻ったか：区間の属する待ちの続きを抜けた後に最初に始まる phase が `worker`・`revise`・`resume` なら真、`landing`・`ended` などなら偽。待ちの続きが t₀ までに終わらなければ不明。次の種類の待ちに移るのは「次に始まるもの」に数えず、その先で決める。
- turn の上限で止まった：turn の中で runtime の `exit_requested` があり、`turn_finished` の `duration_ms` が無く、長さが 3500 秒以上のもの（resume_timeout は観測区間の途中で 3600 秒から 7200 秒になった）。900 秒の黙りで止めた 1 件は上限に数えない。
- 消した後の最初の turn：削除の当たる待ちの続きが終わった後（待ちの外の削除は記録の後）に最初に始まる turn。
- 次の turn が無い行：run が t₀ までに終わったか、待ちが着地に進んだ（戻らなかった）なら「無し」で決まり、そうでなければ不明。
- また消された：消した後の最初の turn の終わりから、次の turn の始まり（無ければ run の終わり）までに削除がある。次の turn も run の終わりも t₀ までに無ければ不明。

#### 削除の時刻

`build_outputs_removed` の記録の時刻は、実際に消した時刻ではない。
掃除の job は候補ごとに消してから予約を外し（`cleanup::pass_candidates`）、supervisor は job が終わった後に `cleanup::poll_cleanup` の `record_cleanup` で event をまとめて記録する。
そのため、予約が外れた後に着地や resume が始まると、正常な排他のままでも phase の変わりが削除の event より先に記録される。
job の始まりの event は無く、job の長さは記録から決まらない。

そこで、削除の時刻 d を次の範囲に置いた。

- 上限 hi：記録の時刻。ただし記録が待ちの外（待ちの続きが終わった後 600 秒以内）にあるときは、その待ちの続きの終わり。予約の間は着地も resume も始まらないので、削除は待ちの中で起きた。この形の行は 22 件（disk_space 20 件、run_ended 2 件）で、待ちの終わりから記録までは 19 件が 2.1 秒以内、ほかは 5.4・35.9・44.3 秒だった。
- 候補の区間：その待ちの続きの区間のうち、hi までに始まっていた区間だけ。ask の付いた削除は、その ask の区間（ask の作成から答えか close まで）に、ask の無い削除は ask の無い区間に当てる。候補が無い行と、候補が 2 つ以上の種類にまたがる行は種類を「unknown:…」として、e・残りの待ちを不明にした（10 件。どれも ask の無い削除で、候補の区間が 2 つの種類にまたがるもの）。
- d は候補の区間の中で hi 以前のどこか。e は [d の最小 − 起点, d の最大 − 起点]、消してから待ちの終わりまでは [待ちの終わり − d の最大, 待ちの終わり − d の最小] の範囲で出した。削除の時の保留は d の範囲の両端で同じで、その間に変わらなければ決まりとした。
- 「10 分で保留が解けた」は、どの d でも d の後 600 秒以内に `claim_resumed`（disk_space）が来れば真、どの d でも来なければ偽、それ以外は不明。

```python
# ana3.py: ev/<run>.json（dagq events --full --run R --all --until t0、run の始まりから）と k-claim_*.json から
# 待ちの区間（waits2.json）、turn（turns2.json）、削除の行（q02.json）を作る。
# - 待ちの区間: 待ちの phase の続きを、phase の変わり目と、ask の作成・答え・close の時刻で切り、
#   切った小区間ごとに「その時点で開いている ask の kind」か「phase の名前」で種類を付け、同じ種類・同じ ask の続きをまとめる。
# - 起点: ask の区間は ask の作成、ask の無い区間はその区間の始まり以前で最後の turn_finished（run の始まりまで遡る）。無ければ不明。
# - 削除の時刻: event の記録時刻 hi は上限（掃除の job は消して予約を外した後、job の終わりにまとめて記録する）。
#   候補の区間は、記録の時刻（待ちの後の記録なら待ちの終わり）までに始まっていた区間に限り、ask の付いた削除はその ask の区間、
#   ask の無い削除は ask の無い区間に当てる。実際の削除はその区間の中で、記録の時刻以前のどこか。
import json, glob, bisect
from datetime import datetime
def ts(s): return datetime.fromisoformat(s.replace('Z', '+00:00')).timestamp()
T0 = ts(open('t0').read().strip()); LEFT = T0 - 14 * 86400
WAIT = {'landing_answer', 'waiting', 'needs_session', 'landing_queue', 'awaiting_slot', 'held', 'review_held', 'returning', 'awaiting_e2e'}
WORK = {'worker', 'revise', 'resume'}
LIMIT_MIN = 3500   # runtime の exit_requested で終わり duration_ms の無い turn のうち、この秒以上を上限での停止とする
NEAR_END = 600     # 待ちの外で記録された削除は、この秒以内に終わった直前の待ちの続きの中で消したとみなす
hold = []
for k, v in (('claim_held', 1), ('claim_resumed', 0)):
    for e in json.load(open(f'k-{k}.json'))['events']:
        if e['payload'].get('reason') == 'disk_space': hold.append((ts(e['created_at']), v))
hold.sort(); hold_t = [h[0] for h in hold]
def held_at(t):
    i = bisect.bisect_right(hold_t, t) - 1
    return i >= 0 and hold[i][1] == 1
def held_range(lo, hi):
    a, b = held_at(lo), held_at(hi)
    changed = any(lo < h[0] <= hi for h in hold)
    if a and b and not changed: return True
    if not a and not b and not changed: return False
    return None
def resumed_in(a, b): return any(h[1] == 0 and a < h[0] <= b for h in hold)
def resolved10(lo, hi):
    # 削除 d ∈ [lo, hi] の後 600 秒以内に claim_resumed（disk_space）が来たか。どの d でも来たなら真、どの d でも来ないなら偽
    if resumed_in(hi, lo + 600): return True
    if not resumed_in(lo, min(hi + 600, T0)):
        return False if hi + 600 <= T0 else None
    return None
q0, waits, turns = [], [], []
for f in sorted(glob.glob('ev/*.json')):
    d = sorted(json.load(open(f))['events'], key=lambda e: e['id'])
    if not d: continue
    run = d[0]['run_id']; task = d[0]['task_id']
    askkind, askspan = {}, {}
    for e in d:
        p = e['payload'] if isinstance(e['payload'], dict) else {}
        a = p.get('ask_id')
        if not a or e['kind'] not in ('ask_opened', 'ask_answered', 'ask_closed'): continue
        if p.get('kind') and not askkind.get(a): askkind[a] = p['kind']
        if e['kind'] == 'ask_opened': askspan[a] = [ts(e['created_at']), None]
        elif a in askspan and askspan[a][1] is None: askspan[a][1] = ts(e['created_at'])  # 答えか close の早いほう
    ph = [(ts(e['created_at']), e['payload']['phase']) for e in d if e['kind'] == 'run_phase_changed']
    ended_at = next((p[0] for p in ph if p[1] == 'ended'), None)
    # turn
    T = []; st = None; ex = False
    for e in d:
        k = e['kind']
        if k == 'turn_started':
            if st: T.append([st, None, None])
            st = e; ex = False
        elif k == 'exit_requested' and st: ex = True
        elif k == 'turn_finished' and st:
            L = ts(e['created_at']) - ts(st['created_at'])
            T.append([st, e, bool(ex and L >= LIMIT_MIN and e['payload'].get('duration_ms') is None)]); st = None
    if st: T.append([st, None, None])
    pk = [(ts(e['created_at']), (e['payload'].get('attempt') or {}).get('kind')) for e in d if e['kind'] == 'run_phase_changed']
    pk_t = [x[0] for x in pk]
    def phase_kind_at(t):
        i = bisect.bisect_right(pk_t, t) - 1
        return pk[i][1] if i >= 0 else None
    TU = []
    for st_, en, lim in T:
        s = ts(st_['created_at']); e_ = ts(en['created_at']) if en else None
        TU.append(dict(run=run, task=task, s=s, e=e_, L=(e_ - s) if e_ else None, lb=(T0 - s) if e_ is None else None,
                       limit=lim, kind=phase_kind_at(s + 0.001), after_removal=False))
    # turn_finished の時刻（turn の終わりの event そのもの）
    fin = sorted(ts(e['created_at']) for e in d if e['kind'] == 'turn_finished')
    def last_finished(t):
        i = bisect.bisect_right(fin, t) - 1
        return fin[i] if i >= 0 else None
    def last_turn_limit(t):
        prev = [u for u in TU if u['e'] and u['e'] <= t + 1]
        return bool(prev) and bool(max(prev, key=lambda u: u['e'])['limit'])
    def open_at(t):
        # 時刻 t に開いている ask（作成 ≦ t < 答えか close）
        return sorted((o, a) for a, (o, c) in askspan.items() if o <= t and (c is None or t < c))
    segs = []; chains = []
    i = 0
    while i < len(ph):
        if ph[i][1] not in WAIT: i += 1; continue
        j = i
        while j + 1 < len(ph) and ph[j + 1][1] in WAIT: j += 1
        cs_, ce_ = ph[i][0], (ph[j + 1][0] if j + 1 < len(ph) else None)
        after = ph[j + 1][1] if j + 1 < len(ph) else None
        chains.append(dict(s=cs_, e=ce_, after=after))
        end = ce_ if ce_ is not None else T0
        cuts = sorted({p[0] for p in ph[i:j + 1]} | {x for o, c in askspan.values() for x in (o, c) if x is not None and cs_ < x < end})
        cs = []
        for k_, b in enumerate(cuts):
            be = cuts[k_ + 1] if k_ + 1 < len(cuts) else ce_
            name = ph[bisect.bisect_right([p[0] for p in ph], b) - 1][1]
            oa = open_at(b)
            if oa:
                a = oa[0][1]; r = 'answer:' + (askkind.get(a) or 'unknown')
            else:
                a = None
                if name in ('landing_answer', 'waiting'): r = 'landing_answer_no_ask' if name == 'landing_answer' else 'waiting_no_ask'
                elif name in ('awaiting_slot', 'landing_queue'): r = 'landing_slot'
                elif name == 'needs_session': r = 'resume_after_limit' if last_turn_limit(b) else 'needs_session_other'
                else: r = 'other:' + name
            key = (r, a)
            if cs and cs[-1]['key'] == key: cs[-1]['e'] = be; cs[-1]['phases'].append(name)
            else: cs.append(dict(run=run, task=task, s=b, e=be, r=r, key=key, ask=a, phases=[name]))
        for sg in cs:
            # 待ちの終わり（description の「答え・resume・着地の始まり」）: ask の区間は区間の終わり（答えか close）。
            # ask の無い区間は、区間の始まりより後で最初の ask の答え・close と、待ちの続きの終わり（resume・着地などの始まり）の早いほう。
            # ask の作成や phase の種類の変わり目では待ちは終わらない。
            if sg['ask']: sg['wend'] = sg['e']
            else:
                ends = [c for o, c in askspan.values() if c is not None and sg['s'] < c and (ce_ is None or c <= ce_)]
                sg['wend'] = min(ends) if ends else ce_
            if sg['ask']:
                o = askspan[sg['ask']][0]; sg['origin_kind'] = 'ask_opened'
            else:
                o = last_finished(sg['s'])
                sg['origin_kind'] = 'turn_finished' if o is not None else 'none'
            sg['origin'] = o
            sg['L'] = (sg['e'] - o) if (o is not None and sg['e'] is not None) else None
            sg['lb'] = (T0 - o) if (o is not None and sg['e'] is None) else None
            # 戻ったか: この区間の属する待ちの続きを抜けた後に最初に始まる phase が worker の turn か
            sg['returned'] = None if (ce_ is None or after is None) else (after in WORK)
            sg['chain_s'] = cs_; sg['chain_e'] = ce_
            sg.pop('key')
        segs += cs
        i = j + 1
    waits += segs
    # 削除
    rem = sorted((ts(e['created_at']), e) for e in d if e['kind'] == 'build_outputs_removed')
    for hi, e in rem:
        p = e['payload']; a = p.get('ask_id')
        inw = [sg for sg in segs if sg['chain_s'] <= hi and (sg['chain_e'] is None or hi < sg['chain_e'])]
        outside = False
        if not inw:
            near = [sg for sg in segs if sg['chain_e'] is not None and sg['chain_e'] <= hi <= sg['chain_e'] + NEAR_END]
            if near:
                ce = max(sg['chain_e'] for sg in near); inw = [sg for sg in near if sg['chain_e'] == ce]; outside = True
        row = dict(run=run, task=task, at=e['created_at'], hi=hi, reason=p.get('reason'), ask=a, ask_kind=askkind.get(a), gib=p['bytes'] / 2**30, outside=outside, seg=None)
        if not inw:
            row.update(r='not_waiting', lo=hi, hi_eff=hi, ncand=0)
        else:
            hi_eff = min(hi, inw[0]['chain_e']) if inw[0]['chain_e'] is not None else hi
            started = [sg for sg in inw if sg['s'] <= hi_eff]   # 記録の時刻までに始まっていた区間だけ
            cand = [sg for sg in started if sg['ask'] == a] if a else [sg for sg in started if not sg['ask']]
            row['hi_eff'] = hi_eff; row['ncand'] = len(cand)
            if not cand:
                row.update(r='unknown:no_matching_segment', lo=hi_eff)
            else:
                types = sorted({sg['r'] for sg in cand})
                # 区間ごとの削除の時刻の範囲 [max(区間の始まり), min(区間の終わり, 記録の時刻)]
                rs = [(sg, sg['s'], min(sg['e'] if sg['e'] is not None else T0, hi_eff)) for sg in cand]
                row['lo'] = min(x[1] for x in rs)
                if len(types) > 1:
                    row.update(r='unknown:' + '|'.join(types))
                else:
                    row['r'] = types[0]
                    sg0 = max(cand, key=lambda sg: sg['s'])
                    row['seg'] = dict(s=sg0['s'], e=sg0['e'], r=sg0['r'], origin=sg0['origin'], origin_kind=sg0['origin_kind'], phases=sg0['phases'], n=len(cand))
                    if any(sg['origin'] is None for sg in cand):
                        row['e_lo'] = row['e_hi'] = None; row['origin_unknown'] = True
                    else:
                        row['e_lo'] = max(0.0, min(lo_ - sg['origin'] for sg, lo_, _ in rs)); row['e_hi'] = max(hi_ - sg['origin'] for sg, _, hi_ in rs)
                    unf = any(sg['wend'] is None for sg in cand)
                    row['to_end_lo'] = max(0.0, min((sg['wend'] if sg['wend'] is not None else T0) - hi_ for sg, _, hi_ in rs))
                    row['to_end_hi'] = None if unf else max(sg['wend'] - lo_ for sg, lo_, _ in rs)
                    row['unfinished'] = unf
                    rets = {sg['returned'] for sg in cand}
                    row['returned'] = rets.pop() if len(rets) == 1 else None
                    row['short10'] = True if (row['to_end_hi'] is not None and row['to_end_hi'] <= 600) else (False if row['to_end_lo'] > 600 else None)
        for k in ('e_lo', 'e_hi', 'to_end_lo', 'to_end_hi', 'unfinished', 'returned', 'short10'):
            row.setdefault(k, None)
        row.setdefault('origin_unknown', False)
        row['held'] = held_range(row['lo'], row['hi_eff'])
        row['resolved10'] = resolved10(row['lo'], row['hi_eff']) if row['held'] is not False else None
        # 消した後の最初の turn: 削除の当たる待ちの続きが終わった後（待ちの外なら記録の後）に最初に始まる turn
        if inw:
            ce = inw[0]['chain_e']
            start_after = ce
        else:
            start_after = hi
        nt = None if start_after is None else next((t for t in TU if t['s'] >= start_after - 1), None)
        row['has_next'] = bool(nt)
        if nt: nt['after_removal'] = True
        row['next_turn'] = nt['L'] if nt else None; row['next_turn_lb'] = nt['lb'] if nt else None
        row['next_limit'] = nt['limit'] if nt else None
        if nt:
            if nt['e'] is None: row['removed_again'] = None
            else:
                nt2 = next((t for t in TU if t['s'] > nt['e']), None)
                again = [x for x, _ in rem if x > nt['e'] and (nt2 is None or x < nt2['s'] + 1)]
                if again: row['removed_again'] = True
                elif nt2 is not None or ended_at is not None: row['removed_again'] = False
                else: row['removed_again'] = None
            row['next_state'] = 'turn'
        else:
            # 次の turn が無い: run が t0 までに終わったか、待ちが着地に進んだ（戻らなかった）なら「turn 無し」で決まり、そうでなければ不明
            chain_returned = (inw[0]['returned'] if inw else None)
            if ended_at is not None or chain_returned is False: row['next_state'] = 'none'
            else: row['next_state'] = 'unknown'
            row['removed_again'] = None
        q0.append(row)
    turns += TU
json.dump(q0, open('q02.json', 'w'), indent=0); json.dump(waits, open('waits2.json', 'w')); json.dump(turns, open('turns2.json', 'w'))
print(len(q0), len(waits), len(turns))
```

Q2 の表は q2b.py（ana3.py の区間を読む。起点が観測区間の中にあるものだけ。`ORIGIN=start` で参考の値、`WAITS` で起点の無い区間を両端に置いた入力）、Q0 の表・turn の比べ・disk_space の除外は rep2.py が出した。文書の数は全部この 2 本の出力から写した。

```python
# q2b.py G_I G_A G_RUN DT_I DT_A : waits2.json の待ちの区間（起点が観測区間の中のもの）から n・R⁻・R⁺・p⁺・p⁻・M⁻・M⁺ の表と e_r を出す
import json, sys, math
from datetime import datetime
T0 = datetime.fromisoformat(open('t0').read().strip().replace('Z', '+00:00')).timestamp(); LEFT = T0 - 14 * 86400
GI, GA, GRUN, DTI, DTA = map(float, sys.argv[1:6])
import os
ALL = json.load(open(os.environ.get('WAITS', 'waits2.json')))
OUTNAME = 'q22.json'
if os.environ.get('ORIGIN') == 'start':
    # 参考: 起点を区間そのものの始まり（その種類の待ちに入った時刻）に置き換える
    OUTNAME = 'q22s.json'
    for w in ALL:
        w['origin'] = w['s']; w['L'] = (w['e'] - w['s']) if w['e'] is not None else None; w['lb'] = (T0 - w['s']) if w['e'] is None else None
W = [w for w in ALL if w['origin'] is not None and LEFT <= w['origin'] < T0]
# 起点が記録に無い区間（ask が無く、区間の前に turn_finished が無い）は e と L が決まらないので外し、種類ごとの件数を出す
NOORIG = {}
for w in ALL:
    if w['origin'] is None and LEFT <= w['s'] < T0: NOORIG[w['r']] = NOORIG.get(w['r'], 0) + 1
LEFTOUT = sum(1 for w in ALL if w['origin'] is not None and w['origin'] < LEFT and (w['e'] is None or w['e'] >= LEFT))
def pct(xs, p):
    xs = sorted(xs)
    if not xs: return None
    k = (len(xs) - 1) * p; f = int(k); c = min(f + 1, len(xs) - 1)
    a, b = xs[f], xs[c]
    if math.isinf(a) or math.isinf(b): return a if k == f else math.inf
    return a + (b - a) * (k - f)
GRID = [0, 5, 10, 15, 30, 60, 120]
def M(G, DT, R, p):
    if G <= 0: return 0.0
    if DT <= 0 or p == 0: return math.inf
    return (G / GRUN) * R / (DT * p)
types = sorted({w['r'] for w in W}, key=lambda r: -sum(1 for w in W if w['r'] == r))
out = {}
for r in types:
    ws = [w for w in W if w['r'] == r]; rows = []
    for e in GRID:
        es = e * 60
        done = [w for w in ws if w['L'] is not None and w['L'] > es]
        unf = [w for w in ws if w['L'] is None and w['lb'] > es]
        unf_low = sum(1 for w in ws if w['L'] is None and w['lb'] <= es)
        n = len(done) + len(unf)
        if n == 0: rows.append(dict(e=e, n=0, unf=len(unf), unf_low=unf_low)); continue
        rm = [w['L'] - es for w in done] + [w['lb'] - es for w in unf]
        rp = [w['L'] - es for w in done] + [math.inf for w in unf]
        Rm, Rp = pct(rm, .5), pct(rp, .5); R90 = pct(rm, .9)
        ret = [w for w in done + unf]
        known = [w['returned'] for w in ret if w['returned'] is not None]; unk = sum(1 for w in ret if w['returned'] is None)
        pp = (sum(known) + unk) / n; pm = sum(known) / n
        row = dict(e=e, n=n, unf=len(unf), unf_low=unf_low, unk=unk, Rm=Rm, Rp=Rp, R90=R90, pp=pp, pm=pm)
        for X, G, DT in (('I', GI, DTI), ('A', GA, DTA)):
            mm = M(G, DT, Rm, pp); mp = M(G, DT, Rp, pm)
            row['M-' + X] = mm; row['M+' + X] = mp
            row['v' + X] = '見合う' if mm >= 2 else ('見合わない' if mp < 2 else '不明')
        rows.append(row)
    res = {}
    for X in ('I', 'A'):
        er = None; unresolved = False
        for row in rows:
            if row['n'] < 10: continue
            if row['v' + X] == '見合う': er = row['e']; break
            if row['v' + X] == '不明': unresolved = True
        allno = all(row['v' + X] == '見合わない' for row in rows if row['n'] >= 10) and any(row['n'] >= 10 for row in rows)
        res[X] = dict(er=er, unresolved=unresolved and er is not None or (er is None and any(row.get('v' + X) == '不明' for row in rows if row['n'] >= 10)), allno=allno)
    out[r] = dict(rows=rows, res=res, n0=rows[0]['n'])
json.dump(out, open(OUTNAME, 'w'), indent=1, default=str)
print('NOORIG', NOORIG, 'LEFTOUT', LEFTOUT, 'N', len(W), 'unfinished', sum(1 for w in W if w['e'] is None), 'ret_unknown', sum(1 for w in W if w['returned'] is None))
def f(x):
    if x is None: return '—'
    if isinstance(x, float):
        if math.isinf(x): return '∞'
        return f'{x:.2f}'
    return str(x)
for r, v in out.items():
    print('##', r, 'n0=', v['n0'], v['res'])
    for row in v['rows']:
        if row['n'] == 0: print(row); continue
        print(row['e'], row['n'], row['unf'], row['unf_low'], round(row['Rm']), f(row['Rp']), round(row['R90']), f(row['pp']), f(row['pm']), f(row['M-I']), f(row['M+I']), row['vI'], f(row['M-A']), f(row['M+A']), row['vA'])
```

```python
# rep2.py: q02.json・turns2.json・q22.json から、Q0 の表（q0.md）、turn の比べ（turns.md）、
# disk_space の除外の条件の 2 通りの集計（ds.md・dsrows.md）と、Q2 の表（q2.md）を出す。数は全部この script の出力から文書に写す。
import json, math, collections
from datetime import datetime
T0 = datetime.fromisoformat(open('t0').read().strip().replace('Z', '+00:00')).timestamp(); LEFT = T0 - 14 * 86400
q = sorted([r for r in json.load(open('q02.json')) if r['hi'] >= LEFT], key=lambda r: r['hi'])
TU = json.load(open('turns2.json')); Q2 = json.load(open('q22.json'))
def b(x): return '不明' if x is None else ('真' if x else '偽')
def rng(lo, hi):
    if lo is None and hi is None: return '—'
    if hi is None: return f'≥ {round(lo)}（未終了）'
    return f'{round(lo)}' if round(lo) == round(hi) else f'{round(lo)}〜{round(hi)}'
def harm_again(r):
    if r['next_state'] == 'none': return False
    if r['next_state'] == 'unknown': return None
    if r['next_limit'] or r['removed_again']: return True
    if r['next_limit'] is None or r['removed_again'] is None: return None
    return False
def or3(a, c):
    if a or c: return True
    if a is None or c is None: return None
    return False
# --- Q0 の表
L = ['| # | 記録の時刻（UTC） | task | run | reason | ask（kind） | GiB | 種類 r | 記録が待ちの後 | e 秒（起点） | 消してから待ちの終わり 秒 | 保留 | 10 分で保留が解けた | 戻った | 次の turn 秒 | 上限で止まった | また消された |', '|' + '---|' * 17]
for i, r in enumerate(q, 1):
    sg = r['seg']; ok = sg['origin_kind'] if sg else ''
    e = rng(r['e_lo'], r['e_hi']) + (f'（{ok}）' if sg else '')
    te = rng(r['to_end_lo'], r['to_end_hi']) if sg else '—'
    if r['next_state'] == 'turn':
        nt = str(round(r['next_turn'])) if r['next_turn'] is not None else f"≥ {round(r['next_turn_lb'])}（未終了）"
        lim = b(r['next_limit']); ag = b(r['removed_again'])
    elif r['next_state'] == 'none': nt, lim, ag = '無し', '—', '—'
    else: nt, lim, ag = '不明', '不明', '不明'
    ask = f"{r['ask']}（{r['ask_kind']}）" if r['ask'] else '—'
    L.append(f"| {i} | {r['at'][5:19]} | {r['task']} | {r['run'][:8]} | {r['reason']} | {ask} | {r['gib']:.1f} | {r['r']} | {'真' if r['outside'] else '偽'} | {e} | {te} | {b(r['held'])} | {b(r['resolved10']) if r['held'] is not False else '—'} | {b(r['returned'])} | {nt} | {lim} | {ag} |")
open('q0.md', 'w').write('\n'.join(L) + '\n')
print('Q0 rows', len(q))
print('types', sorted(collections.Counter((r['reason'], r['r']) for r in q).items(), key=lambda x: -x[1]))
print('outside', sum(r['outside'] for r in q), collections.Counter(r['reason'] for r in q if r['outside']))
print('unfinished seg', sum(1 for r in q if r['unfinished']), 'unknown type', sum(1 for r in q if r['r'].startswith('unknown')), 'not_waiting', sum(1 for r in q if r['r'] == 'not_waiting'))
print('next_state', collections.Counter(r['next_state'] for r in q))
print('next_limit', collections.Counter(r['next_limit'] for r in q if r['next_state'] == 'turn'), 'again', collections.Counter(r['removed_again'] for r in q if r['next_state'] == 'turn'))
cyc = [r for r in q if r['next_limit'] and r['removed_again']]
print('cycle', [(r['task'], r['at'][:19]) for r in cyc], 'cycle unknown', sum(1 for r in q if r['next_state'] == 'unknown' or (r['next_state'] == 'turn' and (r['next_limit'] is None or (r['next_limit'] and r['removed_again'] is None)))))
# --- turn の比べ
def pct(xs, p):
    xs = sorted(xs)
    if not xs: return None
    k = (len(xs) - 1) * p; f = int(k); c = min(f + 1, len(xs) - 1)
    if math.isinf(xs[c]): return xs[f] if k == f else math.inf
    return xs[f] + (xs[c] - xs[f]) * (k - f)
lines = ['| 組 | 件数 | 決まった件数 | 未終了（経過 秒） | 中央値 秒 | 90% 秒 | 上限で止まった | 割合（決まった行） |', '|---|---|---|---|---|---|---|---|']
G = {}
for label, kinds in (('resume', ('resume',)), ('revise', ('revise',)), ('revise・resume', ('resume', 'revise'))):
    for ar in (True, False):
        g = [t for t in TU if LEFT <= t['s'] < T0 and t['kind'] in kinds and t['after_removal'] == ar]
        done = [t for t in g if t['L'] is not None]; unf = [t for t in g if t['L'] is None]
        lim = sum(1 for t in done if t['limit'])
        lines.append(f"| {label}・{'消した後' if ar else '消していない'} | {len(g)} | {len(done)} | {len(unf)}（{', '.join(str(round(t['lb'])) for t in unf) or '—'}） | {round(pct([t['L'] for t in done], .5))} | {round(pct([t['L'] for t in done], .9))} | {lim} | {100 * lim / len(done):.1f}% |")
        G[(label, ar)] = (g, done, unf, lim)
open('turns.md', 'w').write('\n'.join(lines) + '\n')
for label in ('resume', 'revise・resume'):
    gA, dA, uA, lA = G[(label, True)]; gB, dB, uB, lB = G[(label, False)]
    m1 = pct([t['L'] for t in dA] + [t['lb'] for t in uA], .5) - pct([t['L'] for t in dB] + [math.inf] * len(uB), .5)
    m2 = pct([t['L'] for t in dA] + [math.inf] * len(uA), .5) - pct([t['L'] for t in dB] + [t['lb'] for t in uB], .5)
    lo = 100 * lA / len(gA) - 100 * (lB + len(uB)) / len(gB); hi = 100 * (lA + len(uA)) / len(gA) - 100 * lB / len(gB)
    print('turn', label, 'median diff', m1, m2, 'limit pt', round(lo, 2), round(hi, 2))
print('left turns', sum(1 for t in TU if t['s'] < LEFT and (t['e'] is None or t['e'] >= LEFT)))
# --- e_r(A)
ER = {}
for r_, v in Q2.items():
    a = v['res']['A']
    ER[r_] = a['er'] if a['er'] is not None else ('none' if a['allno'] else 'unresolved')
print('ER', ER)
# --- disk_space の除外
ds = [r for r in q if r['reason'] == 'disk_space']
rows = []
for r in ds:
    h = or3(r['short10'], harm_again(r))
    good = False if r['held'] is False else (r['resolved10'] if r['held'] else (True if r['resolved10'] else None))
    rows.append(dict(r=r, harm=h, short=r['short10'], again=harm_again(r), good=good))
def ge(r, secs):
    if r['e_lo'] is None: return None
    if r['e_lo'] >= secs: return True
    if r['e_hi'] < secs: return False
    return None
def c1(r):
    e = ER.get(r['r'])
    if not isinstance(e, int): return False
    return ge(r, e * 60)
CONDS = [
    ('C1', 'e_r(A) を disk_space にも当てる（e_r の出ない種類・種類の決まらない行・待ちの外は消さない）', c1),
    ('C2', 'turn の上限で止まった後の resume の待ちを除く', lambda r: r['r'] != 'resume_after_limit' if not r['r'].startswith('unknown') else (None if 'resume_after_limit' in r['r'] else True)),
    ('C3', 'needs_session の待ち（resume の直前）を全部除く', lambda r: (not r['r'].startswith(('needs_session', 'resume_after_limit'))) if not r['r'].startswith('unknown') else (None if ('needs_session' in r['r'] or 'resume_after_limit' in r['r']) else True)),
    ('C4', '待ちに入って 10 分以上の run だけ消す（待ちの長い順の代わり）', lambda r: ge(r, 600)),
    ('C5', 'C3 かつ e ≧ 5 分', None),
]
def c5(r):
    a = CONDS[2][2](r); c = ge(r, 300)
    if a is False or c is False: return False
    if a is None or c is None: return None
    return True
CONDS[4] = ('C5', CONDS[4][1], c5)
print('ds', len(ds), 'harm', collections.Counter(x['harm'] for x in rows), 'short', collections.Counter(x['short'] for x in rows), 'again', collections.Counter(x['again'] for x in rows), 'good', collections.Counter(x['good'] for x in rows))
out = []
for scen in ('S1', 'S2'):
    # S1: 不明の除く対象は全部当たる・不明の保留は全部解けた・条件の当てはめの不明は除く側。S2: その逆
    for code, name, f in CONDS:
        keep = []; drop = []
        for x in rows:
            k = f(x['r'])
            if k is None: k = (scen == 'S2')
            (keep if k else drop).append(x)
        hv = lambda x: x['harm'] if x['harm'] is not None else (scen == 'S1')
        gv = lambda x: x['good'] if x['good'] is not None else (scen == 'S1')
        out.append(dict(scen=scen, code=code, name=name, drop=len(drop), harm_drop=sum(hv(x) for x in drop), harm_keep=sum(hv(x) for x in keep),
                        good_keep=sum(gv(x) for x in keep), good_drop=sum(gv(x) for x in drop), good_total=sum(gv(x) for x in rows), harm_total=sum(hv(x) for x in rows)))
for o in out: print(o)
for scen in ('S1', 'S2'):
    ok = [o for o in out if o['scen'] == scen and o['good_drop'] == 0]
    best = max(ok, key=lambda o: o['harm_drop']) if ok else None
    print('best', scen, best and best['code'], [o['code'] for o in ok])
T = ['| 置き方 | 条件 | 除く件 | うち除く対象 | 除く対象で残る件 | 保留の解けた件で残す | 保留の解けた件で除く |', '|---|---|---|---|---|---|---|']
for o in out: T.append(f"| {o['scen']} | {o['code']} {o['name']} | {o['drop']} | {o['harm_drop']} | {o['harm_keep']} | {o['good_keep']} | {o['good_drop']} |")
open('ds.md', 'w').write('\n'.join(T) + '\n')
R = ['| 記録の時刻（UTC） | task | 種類 r | e 秒 | 10 分以内に待ちが終わった | また消された・上限 | 保留が 10 分で解けた | C1 | C2 | C3 | C4 | C5 |', '|---|---|---|---|---|---|---|---|---|---|---|---|']
for x in rows:
    r = x['r']; ks = [f(r) for _, _, f in CONDS]
    if all(k is True for k in ks) and x['harm'] is False and x['good'] is False: continue
    R.append(f"| {r['at'][:19]} | {r['task']} | {r['r']} | {rng(r['e_lo'], r['e_hi'])} | {b(x['short'])} | {b(x['again'])} | {b(x['good'])} | " + ' | '.join({True: '残す', False: '除く', None: '不明'}[k] for k in ks) + ' |')
open('dsrows.md', 'w').write('\n'.join(R) + '\n')
print('dsrows', len(R) - 2)
# --- Q2 の表
def ff(x):
    if x is None: return '—'
    x = float(x)
    return '∞' if math.isinf(x) else f'{x:.2f}'
Lq = []
for r_, v in Q2.items():
    Lq.append(f"\n#### {r_}（e = 0 の区間 {v['n0']} 件、e_r(A) = {ER[r_]}）\n")
    Lq.append('| e 分 | n | 未終了（下限 > e） | 未終了（下限 ≦ e、除く） | 戻ったか不明 | R⁻ 秒 | R⁺ 秒 | R の 90% 秒 | p⁺ | p⁻ | M⁻(I) | M⁺(I) | I | M⁻(A) | M⁺(A) | A |')
    Lq.append('|' + '---|' * 16)
    for row in v['rows']:
        if row['n'] == 0:
            Lq.append(f"| {row['e']} | 0 | {row['unf']} | {row['unf_low']} | — | — | — | — | — | — | — | — | — | — | — | — |"); continue
        Lq.append(f"| {row['e']} | {row['n']} | {row['unf']} | {row['unf_low']} | {row['unk']} | {round(float(row['Rm']))} | {ff(row['Rp'])} | {round(float(row['R90']))} | {ff(row['pp'])} | {ff(row['pm'])} | {ff(row['M-I'])} | {ff(row['M+I'])} | {row['vI']} | {ff(row['M-A'])} | {ff(row['M+A'])} | {row['vA']} |")
open('q2.md', 'w').write('\n'.join(Lq) + '\n')
for r_, v in Q2.items():
    row = v['rows'][0]
    if row['n']: print('M0', r_, v['n0'], ff(row['M-A']), ff(row['M+A']), row['vA'], ff(row['M-I']), 'dt674', ff(float(row['M-A']) * 308.5 / 674) if not math.isinf(float(row['M-A'])) else '∞')
```

### 種類 r と記録の名前の対応

| 種類 r | 記録の名前（小区間の始まりに開いている ask の kind と、phase） |
|---|---|
| answer:approve_landing・answer:worker_question・answer:stalled・answer:blocked | その kind の ask が開いている（作成から答えか close まで。phase は主に `landing_answer`・`waiting`・`needs_session`） |
| landing_answer_no_ask | phase `landing_answer` で ask が開いていない（`integration_held` で着地の検証が止まった後、approve_landing の ask が作られるまでと答えの後） |
| waiting_no_ask | phase `waiting` で ask が開いていない（ask の作成の前か、答えの後で phase が変わるまで） |
| landing_slot | phase `awaiting_slot` か `landing_queue` で ask が開いていない（着地 slot の待ち） |
| resume_after_limit | phase `needs_session` で ask が開いておらず、直前の turn が上限で止まった（turn の上限で止まった後の resume の待ち） |
| needs_session_other | phase `needs_session` で ask が開いておらず、上に当たらない（答えの後・差し戻しの後・検証の失敗の後の resume の前） |
| other:returning | phase `returning`（答えが届いて slot を取り直すまで） |
| other:awaiting_e2e | phase `awaiting_e2e`（e2e の slot の待ち） |
| other:held・other:review_held | phase `held`・`review_held` |
| unknown:A\|B | 削除が種類 A と B のどちらの区間に当たるか決まらない（Q0 の行だけ） |
| not_waiting | 記録の時刻とその前 600 秒に待ちの続きが無い削除（run_ended の 18 件。Q0 の行だけ） |

削除の reason と ask の kind は記録の名前のまま（`awaiting_answer`・`disk_space`・`run_ended`）。

## Q0：今の消し方の代償

### 消した 1 件ごとの表

443 件、記録の時刻の順。列は description の Q0 の項目。「記録が待ちの後」は記録が待ちの続きの終わりより後の行（上の「削除の時刻」）。e は範囲（括弧は起点：`ask_opened` は ask の作成、`turn_finished` は直前の turn_finished）、「消してから待ちの終わり」は範囲（待ちの終わりは上の「待ちの区間と種類」の定義）。「保留」は削除の時の disk_space の claim の保留、「10 分で保留が解けた」は保留が偽でない行だけ。

4133（task）の 3 件は 2026-11-13T23:35:44（disk_space、worker_question の ask 1756 の作成から e = 14〜106 秒）と 2026-11-14T00:39:11（disk_space、上限で止まった turn の turn_finished から e = 5〜16 秒）と 2026-11-13T21:00:30 の awaiting_answer（ほかに 01:59:04・02:51:13 の awaiting_answer と、別の run f76f71ae の 11-15T06:43:55 の disk_space）、4134 の 1 件は 2026-11-14T00:19:48 の awaiting_answer。
task 4849 の 2026-11-14T01:22:50 の disk_space は approve_landing の ask 1759 が作られる前の、task 3963 の 2026-11-05T03:46:34 の disk_space は blocked の ask 1504（03:52:14 の作成）が作られる前の、ask の無い landing_answer の区間に当たる（起点は直前の turn_finished で、e はそれぞれ 8309〜8409 秒と 8060〜9353 秒）。4849 の待ちの終わりは ask 1759 の答えで、消してから 101〜200 秒。

<details>
<summary>443 件の表</summary>

| # | 記録の時刻（UTC） | task | run | reason | ask（kind） | GiB | 種類 r | 記録が待ちの後 | e 秒（起点） | 消してから待ちの終わり 秒 | 保留 | 10 分で保留が解けた | 戻った | 次の turn 秒 | 上限で止まった | また消された |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 11-02T18:14:20 | 3968 | 3a9fa943 | awaiting_answer | 1436（approve_landing） | 9.6 | answer:approve_landing | 偽 | 6010〜6052（ask_opened） | 18204〜18246 | 偽 | — | 真 | 642 | 偽 | 偽 |
| 2 | 11-02T18:16:39 | 4007 | df7e4553 | awaiting_answer | 1438（approve_landing） | 11.7 | answer:approve_landing | 偽 | 0〜26（ask_opened） | 18556〜18582 | 偽 | — | 真 | 759 | 偽 | 偽 |
| 3 | 11-02T18:48:29 | 3946 | 2c080f44 | disk_space | — | 7.8 | landing_slot | 偽 | 380〜384（turn_finished） | 1〜4 | 偽 | — | 偽 | 無し | — | — |
| 4 | 11-02T19:07:11 | 3976 | 88eee852 | awaiting_answer | 1439（approve_landing） | 18.2 | answer:approve_landing | 偽 | 0〜57（ask_opened） | 13254〜13311 | 偽 | — | 偽 | 無し | — | — |
| 5 | 11-02T19:31:27 | 3966 | 5b9dff15 | awaiting_answer | 1441（approve_landing） | 7.9 | answer:approve_landing | 偽 | 0〜34（ask_opened） | 14213〜14247 | 偽 | — | 真 | 401 | 偽 | 偽 |
| 6 | 11-02T20:48:17 | 4018 | c2c8a9bb | awaiting_answer | 1435（approve_landing） | 8.7 | answer:approve_landing | 偽 | 15376〜15418（ask_opened） | 8826〜8868 | 偽 | — | 真 | 660 | 偽 | 真 |
| 7 | 11-02T21:05:39 | 3924 | 5e61c74a | awaiting_answer | 1446（approve_landing） | 10.9 | answer:approve_landing | 偽 | 0〜185（ask_opened） | 9165〜9350 | 偽 | — | 真 | 690 | 偽 | 偽 |
| 8 | 11-03T00:31:37 | 3844 | a72ee398 | awaiting_answer | 1450（approve_landing） | 15.9 | answer:approve_landing | 偽 | 0〜96（ask_opened） | 1267〜1363 | 偽 | — | 真 | 866 | 偽 | 真 |
| 9 | 11-03T00:52:01 | 4018 | c2c8a9bb | awaiting_answer | 1451（approve_landing） | 5.3 | answer:approve_landing | 偽 | 0〜86（ask_opened） | 157〜242 | 偽 | — | 真 | 834 | 偽 | 真 |
| 10 | 11-03T01:41:42 | 4018 | c2c8a9bb | awaiting_answer | 1452（approve_landing） | 7.6 | answer:approve_landing | 偽 | 0〜88（ask_opened） | 714〜801 | 偽 | — | 真 | 677 | 偽 | 真 |
| 11 | 11-03T02:48:16 | 4018 | c2c8a9bb | awaiting_answer | 1453（approve_landing） | 7.5 | answer:approve_landing | 偽 | 0〜2（ask_opened） | 4280〜4282 | 偽 | — | 偽 | 無し | — | — |
| 12 | 11-03T03:08:03 | 3976 | 6580f3ce | awaiting_answer | 1454（approve_landing） | 14.0 | answer:approve_landing | 偽 | 0〜33（ask_opened） | 3164〜3197 | 偽 | — | 真 | 1746 | 偽 | 偽 |
| 13 | 11-03T03:14:53 | 4058 | 0939863e | awaiting_answer | 1455（approve_landing） | 13.4 | answer:approve_landing | 偽 | 0〜17（ask_opened） | 2830〜2846 | 偽 | — | 真 | 881 | 偽 | 偽 |
| 14 | 11-03T06:23:08 | 4018 | 1460a41a | awaiting_answer | 1460（approve_landing） | 5.3 | answer:approve_landing | 偽 | 0〜3（ask_opened） | 251〜253 | 偽 | — | 真 | 583 | 偽 | 偽 |
| 15 | 11-03T07:04:44 | 3976 | 6580f3ce | awaiting_answer | 1462（approve_landing） | 12.9 | answer:approve_landing | 偽 | 0〜37（ask_opened） | 226〜263 | 偽 | — | 真 | 1774 | 偽 | 真 |
| 16 | 11-03T07:41:20 | 3844 | a72ee398 | disk_space | — | 6.3 | unknown:landing_answer_no_ask|needs_session_other | 偽 | — | — | 偽 | — | 不明 | 2559 | 偽 | 真 |
| 17 | 11-03T08:06:55 | 3976 | 6580f3ce | awaiting_answer | 1463（approve_landing） | 11.1 | answer:approve_landing | 偽 | 0〜149（ask_opened） | 178〜327 | 偽 | — | 真 | 2557 | 偽 | 真 |
| 18 | 11-03T09:15:33 | 4027 | 63d35bb2 | awaiting_answer | 1466（approve_landing） | 13.9 | answer:approve_landing | 偽 | 0〜82（ask_opened） | 202〜283 | 偽 | — | 真 | 787 | 偽 | 真 |
| 19 | 11-03T09:35:14 | 4027 | 63d35bb2 | awaiting_answer | 1466（approve_landing） | 5.3 | answer:approve_landing | 偽 | 1261〜1264（ask_opened） | 3826〜3828 | 偽 | — | 偽 | 無し | — | — |
| 20 | 11-03T10:29:29 | 3976 | 6580f3ce | awaiting_answer | 1469（approve_landing） | 9.5 | answer:approve_landing | 偽 | 0〜141（ask_opened） | 872〜1013 | 偽 | — | 偽 | 無し | — | — |
| 21 | 11-03T10:34:20 | 3844 | a72ee398 | awaiting_answer | 1470（approve_landing） | 10.7 | answer:approve_landing | 偽 | 0〜109（ask_opened） | 738〜846 | 偽 | — | 真 | 1617 | 偽 | 偽 |
| 22 | 11-03T12:20:43 | 4118 | 861a9351 | run_ended | — | 1.2 | not_waiting | 偽 | — | — | 偽 | — | 不明 | 132 | 偽 | 偽 |
| 23 | 11-03T13:23:17 | 4016 | 13659313 | disk_space | — | 13.1 | landing_slot | 偽 | 3805〜3810（turn_finished） | 1〜6 | 真 | 真 | 偽 | 無し | — | — |
| 24 | 11-03T17:30:57 | 3984 | dd032f2a | run_ended | — | 7.4 | not_waiting | 偽 | — | — | 偽 | — | 不明 | 6 | 偽 | 偽 |
| 25 | 11-03T18:10:37 | 3976 | 308ebb85 | disk_space | 1474（worker_question） | 8.4 | answer:worker_question | 偽 | 173〜5854（ask_opened） | 17249〜22931 | 偽 | — | 真 | 2261 | 偽 | 真 |
| 26 | 11-03T18:12:16 | 3847 | f6f68c8a | run_ended | — | 5.9 | not_waiting | 偽 | — | — | 偽 | — | 不明 | 無し | — | — |
| 27 | 11-03T18:21:13 | 3846 | 0d5381b3 | run_ended | — | 9.4 | not_waiting | 偽 | — | — | 偽 | — | 不明 | 無し | — | — |
| 28 | 11-03T18:42:32 | 3847 | 0b0d59f5 | run_ended | — | 10.3 | not_waiting | 偽 | — | — | 偽 | — | 不明 | 5 | 偽 | 偽 |
| 29 | 11-03T21:19:21 | 3941 | e044e3a4 | awaiting_answer | 1481（approve_landing） | 8.7 | answer:approve_landing | 偽 | 0〜91（ask_opened） | 6925〜7016 | 偽 | — | 偽 | 無し | — | — |
| 30 | 11-03T22:09:02 | 4015 | 2010e4dd | run_ended | — | 8.9 | not_waiting | 偽 | — | — | 偽 | — | 不明 | 5 | 偽 | 偽 |
| 31 | 11-03T22:30:47 | 4094 | e90d9b22 | disk_space | — | 10.7 | landing_slot | 真 | 932〜1101（turn_finished） | 0〜169 | 偽 | — | 偽 | 無し | — | — |
| 32 | 11-03T22:40:28 | 4029 | ddff8c53 | run_ended | — | 8.7 | not_waiting | 偽 | — | — | 偽 | — | 不明 | 2077 | 偽 | 真 |
| 33 | 11-03T23:23:15 | 4029 | ddff8c53 | run_ended | — | 45.6 | not_waiting | 偽 | — | — | 偽 | — | 不明 | 無し | — | — |
| 34 | 11-04T00:03:12 | 3976 | 308ebb85 | disk_space | 1483（worker_question） | 5.5 | answer:worker_question | 偽 | 4〜206（ask_opened） | 1689〜1891 | 真 | 偽 | 真 | 3985 | 偽 | 偽 |
| 35 | 11-04T00:24:56 | 4081 | cad136a0 | awaiting_answer | 1484（approve_landing） | 15.4 | answer:approve_landing | 偽 | 0〜6（ask_opened） | 7354〜7360 | 真 | 偽 | 偽 | 無し | — | — |
| 36 | 11-04T02:43:25 | 3976 | 308ebb85 | awaiting_answer | 1486（approve_landing） | 14.9 | answer:approve_landing | 偽 | 0〜53（ask_opened） | 685〜738 | 真 | 偽 | 真 | 1290 | 偽 | 真 |
| 37 | 11-04T02:47:39 | 4081 | cad136a0 | disk_space | — | 2.4 | landing_slot | 偽 | 8872〜8943（turn_finished） | 3〜74 | 真 | 偽 | 偽 | 無し | — | — |
| 38 | 11-04T03:23:34 | 3976 | 308ebb85 | disk_space | — | 7.1 | landing_slot | 偽 | 42〜111（turn_finished） | 1868〜1938 | 真 | 偽 | 偽 | 無し | — | — |
| 39 | 11-04T12:17:27 | 4118 | 1669beb6 | run_ended | — | 1.2 | not_waiting | 偽 | — | — | 偽 | — | 不明 | 146 | 偽 | 偽 |
| 40 | 11-04T13:17:53 | 3887 | e0165289 | awaiting_answer | 1494（approve_landing） | 12.5 | answer:approve_landing | 偽 | 0〜155（ask_opened） | 5191〜5346 | 偽 | — | 偽 | 5910 | 偽 | 偽 |
| 41 | 11-04T14:29:31 | 4083 | 8f01f2a2 | awaiting_answer | 1495（approve_landing） | 16.3 | answer:approve_landing | 偽 | 0〜55（ask_opened） | 28546〜28601 | 偽 | — | 偽 | 無し | — | — |
| 42 | 11-04T23:05:28 | 4204 | 2dc6fde7 | run_ended | — | 3.5 | needs_session_other | 偽 | 260〜265（turn_finished） | 365〜370 | 真 | 真 | 真 | 1773 | 偽 | 偽 |
| 43 | 11-04T23:23:51 | 3887 | e0165289 | disk_space | — | 6.8 | landing_slot | 偽 | 716〜1813（turn_finished） | 4832〜5929 | 不明 | 不明 | 偽 | 2033 | 偽 | 偽 |
| 44 | 11-04T23:53:53 | 4161 | eacb20c8 | awaiting_answer | 1499（approve_landing） | 10.8 | answer:approve_landing | 偽 | 0〜150（ask_opened） | 244〜393 | 偽 | — | 偽 | 無し | — | — |
| 45 | 11-05T02:53:57 | 4164 | d3b886de | awaiting_answer | 1503（approve_landing） | 8.9 | answer:approve_landing | 偽 | 0〜254（ask_opened） | 36885〜37139 | 偽 | — | 偽 | 無し | — | — |
| 46 | 11-05T03:46:34 | 3963 | ae632dc5 | disk_space | — | 5.2 | landing_answer_no_ask | 偽 | 8060〜9353（turn_finished） | 14249〜15542 | 偽 | — | 偽 | 無し | — | — |
| 47 | 11-05T07:55:38 | 3923 | 95389de2 | disk_space | — | 7.6 | landing_slot | 偽 | 6587〜7042（turn_finished） | 6451〜6905 | 偽 | — | 偽 | 無し | — | — |
| 48 | 11-05T11:24:44 | 3978 | f629310a | awaiting_answer | 1505（approve_landing） | 9.5 | answer:approve_landing | 偽 | 0〜70（ask_opened） | 6567〜6637 | 偽 | — | 真 | 1477 | 偽 | 偽 |
| 49 | 11-05T16:59:23 | 3978 | f629310a | disk_space | 1507（worker_question） | 12.2 | answer:worker_question | 偽 | 19〜4059（ask_opened） | 22048〜26088 | 偽 | — | 真 | 104 | 偽 | 偽 |
| 50 | 11-05T22:40:42 | 4104 | 717f8205 | awaiting_answer | 1508（approve_landing） | 11.1 | answer:approve_landing | 偽 | 0〜482（ask_opened） | 1793〜2275 | 偽 | — | 偽 | 無し | — | — |
| 51 | 11-06T00:20:31 | 4192 | 548bcd54 | disk_space | — | 7.6 | landing_answer_no_ask | 偽 | 2773〜4519（turn_finished） | 2139〜3885 | 偽 | — | 偽 | 無し | — | — |
| 52 | 11-06T01:24:34 | 4192 | 548bcd54 | disk_space | — | 6.9 | landing_answer_no_ask | 偽 | 8030〜8363（turn_finished） | 9978〜10311 | 偽 | — | 偽 | 無し | — | — |
| 53 | 11-06T02:52:43 | 4084 | aa724835 | disk_space | — | 13.3 | landing_answer_no_ask | 偽 | 2702〜5477（turn_finished） | 403〜3177 | 偽 | — | 偽 | 無し | — | — |
| 54 | 11-06T02:52:43 | 4290 | 38515617 | disk_space | — | 4.3 | landing_slot | 偽 | 2062〜2895（turn_finished） | 2290〜3123 | 偽 | — | 偽 | 無し | — | — |
| 55 | 11-06T05:19:44 | 4168 | 1c538e16 | awaiting_answer | 1518（approve_landing） | 11.0 | answer:approve_landing | 偽 | 0〜4（ask_opened） | 19741〜19745 | 偽 | — | 真 | 1527 | 偽 | 真 |
| 56 | 11-06T05:55:09 | 4308 | 6c8f5068 | awaiting_answer | 1520（approve_landing） | 10.6 | answer:approve_landing | 偽 | 0〜119（ask_opened） | 17757〜17876 | 偽 | — | 偽 | 無し | — | — |
| 57 | 11-06T09:17:04 | 4098 | 83db4edf | run_ended | — | 11.8 | not_waiting | 偽 | — | — | 真 | 真 | 不明 | 2777 | 偽 | 偽 |
| 58 | 11-06T11:22:04 | 4098 | 83db4edf | awaiting_answer | 1524（approve_landing） | 12.9 | answer:approve_landing | 偽 | 0〜5（ask_opened） | 2118〜2124 | 不明 | 不明 | 真 | 1679 | 偽 | 真 |
| 59 | 11-06T11:57:27 | 4085 | edfd4773 | awaiting_answer | 1525（approve_landing） | 9.0 | answer:approve_landing | 偽 | 0〜9（ask_opened） | 3382〜3391 | 偽 | — | 偽 | 無し | — | — |
| 60 | 11-06T12:12:56 | 4099 | 6b7eb1ff | disk_space | 1526（worker_question） | 11.2 | answer:worker_question | 偽 | 49〜55（ask_opened） | 2756〜2761 | 偽 | — | 真 | 83 | 偽 | 偽 |
| 61 | 11-06T12:27:04 | 4168 | 1c538e16 | awaiting_answer | 1527（approve_landing） | 9.7 | answer:approve_landing | 偽 | 0〜91（ask_opened） | 1979〜2070 | 偽 | — | 真 | 786 | 偽 | 真 |
| 62 | 11-06T12:29:53 | 4098 | 83db4edf | awaiting_answer | 1524（approve_landing） | 7.7 | answer:approve_landing | 偽 | 3904〜4074（ask_opened） | 1221〜1391 | 偽 | — | 真 | 1434 | 偽 | 真 |
| 63 | 11-06T13:49:41 | 4096 | 2616f72c | disk_space | — | 7.4 | landing_slot | 偽 | 326〜455（turn_finished） | 8106〜8234 | 真 | 真 | 真 | 1641 | 偽 | 偽 |
| 64 | 11-06T14:25:03 | 4168 | 1c538e16 | disk_space | — | 6.6 | unknown:landing_answer_no_ask|needs_session_other | 偽 | — | — | 偽 | — | 不明 | 1358 | 偽 | 偽 |
| 65 | 11-06T16:01:10 | 4100 | 9d08dd92 | disk_space | — | 8.7 | unknown:landing_slot|needs_session_other | 偽 | — | — | 偽 | — | 不明 | 2045 | 偽 | 真 |
| 66 | 11-06T16:06:42 | 4098 | 83db4edf | awaiting_answer | 1528（approve_landing） | 8.4 | answer:approve_landing | 偽 | 0〜2（ask_opened） | 4601〜4603 | 偽 | — | 真 | 1241 | 偽 | 真 |
| 67 | 11-06T17:11:57 | 4100 | 9d08dd92 | disk_space | — | 6.7 | landing_slot | 偽 | 1521〜1828（turn_finished） | 2316〜2623 | 偽 | — | 真 | 1237 | 偽 | 真 |
| 68 | 11-06T17:50:35 | 4098 | 83db4edf | awaiting_answer | 1528（approve_landing） | 7.8 | answer:approve_landing | 偽 | 6201〜6235（ask_opened） | 8067〜8101 | 不明 | 不明 | 真 | 1930 | 偽 | 真 |
| 69 | 11-06T18:16:08 | 4100 | 9d08dd92 | disk_space | — | 8.8 | landing_slot | 偽 | 290〜297（turn_finished） | 5〜12 | 偽 | — | 偽 | 無し | — | — |
| 70 | 11-06T18:39:21 | 4105 | da2103ee | disk_space | — | 10.4 | landing_slot | 偽 | 463〜473（turn_finished） | 2583〜2593 | 不明 | 不明 | 偽 | 無し | — | — |
| 71 | 11-06T18:49:07 | 4174 | 4a941cbf | awaiting_answer | 1529（approve_landing） | 20.1 | answer:approve_landing | 偽 | 0〜148（ask_opened） | 13591〜13739 | 偽 | — | 真 | 1072 | 偽 | 偽 |
| 72 | 11-06T19:37:08 | 4251 | 6ddc5f8a | disk_space | — | 6.7 | landing_slot | 真 | 310〜424（turn_finished） | 0〜114 | 真 | 偽 | 偽 | 1181 | 偽 | 偽 |
| 73 | 11-06T19:49:35 | 4251 | 6ddc5f8a | disk_space | — | 2.5 | landing_slot | 偽 | 974〜1173（turn_finished） | 3681〜3879 | 真 | 偽 | 真 | 1181 | 偽 | 偽 |
| 74 | 11-06T20:05:06 | 4093 | eee35312 | awaiting_answer | 1530（approve_landing） | 12.0 | answer:approve_landing | 偽 | 0〜74（ask_opened） | 1342〜1416 | 真 | 真 | 真 | 1038 | 偽 | 真 |
| 75 | 11-06T20:50:55 | 4098 | 83db4edf | awaiting_answer | 1528（approve_landing） | 7.8 | answer:approve_landing | 偽 | 17053〜17055（ask_opened） | 5965〜5967 | 真 | 真 | 真 | 1619 | 偽 | 真 |
| 76 | 11-06T20:50:58 | 4093 | eee35312 | awaiting_answer | 1530（approve_landing） | 6.8 | answer:approve_landing | 偽 | 2822〜2826（ask_opened） | 5629〜5634 | 真 | 真 | 真 | 1051 | 偽 | 真 |
| 77 | 11-06T22:01:06 | 4251 | 6ddc5f8a | disk_space | — | 7.2 | landing_slot | 偽 | 701〜769（turn_finished） | 1〜70 | 真 | 偽 | 偽 | 無し | — | — |
| 78 | 11-06T22:08:07 | 4251 | 6ddc5f8a | disk_space | — | 2.5 | landing_slot | 偽 | 1122〜1191（turn_finished） | 2706〜2775 | 真 | 偽 | 偽 | 無し | — | — |
| 79 | 11-06T22:37:01 | 4086 | 61e2700f | disk_space | — | 8.3 | landing_slot | 偽 | 740〜1273（turn_finished） | 972〜1505 | 不明 | 不明 | 偽 | 1285 | 偽 | 偽 |
| 80 | 11-06T23:31:25 | 4093 | eee35312 | disk_space | — | 6.6 | unknown:landing_answer_no_ask|needs_session_other | 偽 | — | — | 偽 | — | 不明 | 983 | 偽 | 真 |
| 81 | 11-06T23:55:51 | 4098 | 83db4edf | disk_space | — | 11.8 | landing_slot | 真 | 221〜228（turn_finished） | 0〜7 | 偽 | — | 偽 | 1043 | 偽 | 真 |
| 82 | 11-07T00:11:34 | 4098 | 83db4edf | disk_space | — | 2.5 | landing_slot | 偽 | 484〜1173（turn_finished） | 4〜692 | 偽 | — | 偽 | 1043 | 偽 | 真 |
| 83 | 11-07T00:33:28 | 4098 | 83db4edf | disk_space | — | 7.8 | landing_slot | 偽 | 10〜25（turn_finished） | 2〜17 | 真 | 真 | 偽 | 無し | — | — |
| 84 | 11-07T00:40:07 | 4098 | 83db4edf | disk_space | — | 2.5 | landing_slot | 偽 | 297〜424（turn_finished） | 6605〜6733 | 真 | 真 | 偽 | 無し | — | — |
| 85 | 11-07T02:30:14 | 4452 | 12e3bfa9 | awaiting_answer | 1533（approve_landing） | 18.4 | answer:approve_landing | 偽 | 0〜7（ask_opened） | 63〜70 | 真 | 真 | 真 | 4439 | 偽 | 真 |
| 86 | 11-07T02:42:20 | 4450 | 8ac7dd40 | disk_space | — | 12.5 | landing_slot | 偽 | 415〜990（turn_finished） | 1387〜1962 | 不明 | 不明 | 偽 | 647 | 偽 | 真 |
| 87 | 11-07T04:16:20 | 4093 | eee35312 | disk_space | 1534（worker_question） | 5.5 | answer:worker_question | 偽 | 557〜752（ask_opened） | 89532〜89727 | 不明 | 真 | 真 | 1914 | 偽 | 偽 |
| 88 | 11-07T04:22:15 | 4452 | 12e3bfa9 | awaiting_answer | 1536（approve_landing） | 13.2 | answer:approve_landing | 偽 | 0〜37（ask_opened） | 89554〜89591 | 偽 | — | 真 | 1580 | 偽 | 真 |
| 89 | 11-07T04:52:45 | 4450 | 8ac7dd40 | disk_space | — | 11.0 | landing_slot | 偽 | 466〜539（turn_finished） | 1401〜1475 | 真 | 真 | 偽 | 無し | — | — |
| 90 | 11-07T07:06:20 | 4445 | bbe6940a | awaiting_answer | 1538（approve_landing） | 13.5 | answer:approve_landing | 偽 | 0〜110（ask_opened） | 1264〜1374 | 不明 | 不明 | 真 | 1778 | 偽 | 真 |
| 91 | 11-07T07:27:26 | 4441 | b79ba274 | run_ended | — | 10.4 | not_waiting | 偽 | — | — | 真 | 真 | 不明 | 2845 | 偽 | 真 |
| 92 | 11-07T07:57:20 | 4445 | bbe6940a | awaiting_answer | 1538（approve_landing） | 8.5 | answer:approve_landing | 偽 | 3165〜3170（ask_opened） | 39198〜39204 | 真 | 偽 | 真 | 1458 | 偽 | 真 |
| 93 | 11-07T08:45:46 | 4441 | b79ba274 | disk_space | 1540（worker_question） | 13.7 | answer:worker_question | 偽 | 2〜44（ask_opened） | 74293〜74335 | 真 | 真 | 真 | 0 | 偽 | 偽 |
| 94 | 11-07T09:18:18 | 4443 | 85bde064 | awaiting_answer | 1542（approve_landing） | 11.6 | answer:approve_landing | 偽 | 0〜5（ask_opened） | 59292〜59298 | 真 | 真 | 真 | 616 | 偽 | 真 |
| 95 | 11-07T09:35:35 | 4070 | 121ede52 | awaiting_answer | 1544（approve_landing） | 11.6 | answer:approve_landing | 偽 | 0〜6（ask_opened） | 14378〜14384 | 真 | 真 | 真 | 1280 | 偽 | 真 |
| 96 | 11-07T09:51:34 | 4182 | e8b828a3 | awaiting_answer | 1545（approve_landing） | 14.5 | answer:approve_landing | 偽 | 0〜4（ask_opened） | 4210〜4214 | 真 | 真 | 真 | 677 | 偽 | 真 |
| 97 | 11-07T10:47:57 | 4297 | dbffdb67 | disk_space | — | 7.3 | landing_slot | 偽 | 143〜200（turn_finished） | 2〜60 | 真 | 真 | 偽 | 無し | — | — |
| 98 | 11-07T10:54:54 | 4237 | 0c1a2fea | disk_space | — | 11.3 | landing_slot | 偽 | 148〜309（turn_finished） | 1474〜1635 | 不明 | 不明 | 偽 | 無し | — | — |
| 99 | 11-07T11:19:24 | 4182 | e8b828a3 | awaiting_answer | 1545（approve_landing） | 5.8 | answer:approve_landing | 偽 | 5146〜5274（ask_opened） | 11673〜11801 | 偽 | — | 真 | 808 | 偽 | 真 |
| 100 | 11-07T12:11:01 | 4082 | 7b5a0994 | awaiting_answer | 1546（approve_landing） | 13.5 | answer:approve_landing | 偽 | 0〜9（ask_opened） | 49047〜49056 | 真 | 真 | 真 | 1309 | 偽 | 真 |
| 101 | 11-07T12:33:15 | 4250 | e70cbf75 | disk_space | — | 7.3 | landing_slot | 偽 | 81〜101（turn_finished） | 1〜21 | 真 | 真 | 偽 | 無し | — | — |
| 102 | 11-07T12:37:34 | 4294 | 67ae4058 | disk_space | — | 8.0 | landing_slot | 偽 | 104〜148（turn_finished） | 1612〜1655 | 真 | 真 | 偽 | 無し | — | — |
| 103 | 11-07T13:13:48 | 4305 | 8d1a61e7 | disk_space | — | 10.4 | landing_slot | 偽 | 1269〜1392（turn_finished） | 2578〜2701 | 不明 | 不明 | 真 | 1170 | 偽 | 真 |
| 104 | 11-07T13:56:45 | 4070 | 121ede52 | awaiting_answer | 1544（approve_landing） | 6.8 | answer:approve_landing | 偽 | 15672〜15676（ask_opened） | 41395〜41399 | 真 | 真 | 真 | 1257 | 偽 | 真 |
| 105 | 11-07T14:05:13 | 4043 | 10577b29 | disk_space | — | 10.1 | landing_slot | 偽 | 390〜549（turn_finished） | 1397〜1555 | 不明 | 不明 | 偽 | 386 | 偽 | 偽 |
| 106 | 11-07T14:33:58 | 4361 | 332fec87 | awaiting_answer | 1547（approve_landing） | 9.4 | answer:approve_landing | 偽 | 0〜99（ask_opened） | 52170〜52268 | 偽 | — | 真 | 1077 | 偽 | 真 |
| 107 | 11-07T14:35:26 | 4305 | 8d1a61e7 | awaiting_answer | 1548（approve_landing） | 9.4 | answer:approve_landing | 偽 | 0〜76（ask_opened） | 41039〜41115 | 偽 | — | 真 | 1363 | 偽 | 真 |
| 108 | 11-07T14:47:37 | 4182 | e8b828a3 | awaiting_answer | 1545（approve_landing） | 5.9 | answer:approve_landing | 偽 | 17766〜17768（ask_opened） | 6952〜6953 | 偽 | — | 真 | 1109 | 偽 | 真 |
| 109 | 11-07T15:33:13 | 4043 | 10577b29 | run_ended | — | 11.5 | not_waiting | 偽 | — | — | 偽 | — | 不明 | 1993 | 偽 | 真 |
| 110 | 11-07T16:08:19 | 4065 | ba9231ac | disk_space | — | 7.9 | landing_slot | 偽 | 752〜1025（turn_finished） | 1628〜1901 | 偽 | — | 偽 | 無し | — | — |
| 111 | 11-07T16:42:46 | 4138 | 85287542 | disk_space | — | 7.5 | landing_slot | 真 | 621〜1050（turn_finished） | 0〜430 | 偽 | — | 偽 | 無し | — | — |
| 112 | 11-07T17:08:32 | 4182 | e8b828a3 | awaiting_answer | 1545（approve_landing） | 8.1 | answer:approve_landing | 偽 | 25994〜26223（ask_opened） | 29924〜30153 | 偽 | — | 真 | 1095 | 偽 | 真 |
| 113 | 11-07T17:52:59 | 4178 | f73f95bc | disk_space | — | 11.9 | landing_slot | 偽 | 278〜579（turn_finished） | 359〜660 | 不明 | 不明 | 偽 | 無し | — | — |
| 114 | 11-07T18:28:34 | 4205 | c4027495 | disk_space | — | 6.2 | landing_slot | 偽 | 753〜2523（turn_finished） | 325〜2095 | 不明 | 不明 | 偽 | 無し | — | — |
| 115 | 11-07T18:47:55 | 4206 | 1d8244cb | disk_space | — | 5.8 | landing_slot | 偽 | 517〜1346（turn_finished） | 122〜952 | 偽 | — | 偽 | 無し | — | — |
| 116 | 11-07T19:16:36 | 4445 | bbe6940a | awaiting_answer | 1538（approve_landing） | 9.2 | answer:approve_landing | 偽 | 43923〜43926（ask_opened） | 21211〜21214 | 偽 | — | 真 | 855 | 偽 | 真 |
| 117 | 11-07T19:45:37 | 4209 | a971c474 | disk_space | — | 9.3 | landing_slot | 偽 | 174〜178（turn_finished） | 89〜94 | 真 | 真 | 偽 | 無し | — | — |
| 118 | 11-07T20:54:43 | 4210 | e7bf9012 | disk_space | — | 6.1 | landing_slot | 偽 | 2290〜2769（turn_finished） | 1305〜1785 | 偽 | — | 真 | 575 | 偽 | 偽 |
| 119 | 11-07T22:01:42 | 4214 | 51f727d8 | disk_space | — | 6.1 | landing_slot | 偽 | 590〜845（turn_finished） | 2898〜3152 | 偽 | — | 偽 | 無し | — | — |
| 120 | 11-07T22:01:42 | 4212 | 570c1db3 | disk_space | — | 6.1 | landing_slot | 偽 | 1418〜1672（turn_finished） | 177〜432 | 偽 | — | 偽 | 無し | — | — |
| 121 | 11-07T22:13:43 | 4177 | 2ac56124 | awaiting_answer | 1535（approve_landing） | 1.3 | answer:approve_landing | 偽 | 64481〜64483（ask_opened） | 25151〜25153 | 偽 | — | 偽 | 無し | — | — |
| 122 | 11-07T23:58:36 | 4213 | 3b7aed85 | disk_space | — | 5.7 | landing_slot | 偽 | 1463〜1715（turn_finished） | 210〜463 | 偽 | — | 偽 | 無し | — | — |
| 123 | 11-08T00:33:52 | 4216 | 4203d008 | disk_space | — | 11.3 | landing_slot | 偽 | 465〜1275（turn_finished） | 360〜1170 | 不明 | 不明 | 偽 | 無し | — | — |
| 124 | 11-08T00:33:52 | 4218 | 3e822c72 | disk_space | — | 9.4 | landing_slot | 偽 | 303〜1448（turn_finished） | 360〜1504 | 不明 | 不明 | 偽 | 無し | — | — |
| 125 | 11-08T01:26:50 | 4445 | bbe6940a | awaiting_answer | 1538（approve_landing） | 6.9 | answer:approve_landing | 偽 | 66138〜66140（ask_opened） | 14132〜14134 | 偽 | — | 真 | 1104 | 偽 | 偽 |
| 126 | 11-08T01:46:32 | 4182 | e8b828a3 | awaiting_answer | 1545（approve_landing） | 5.9 | answer:approve_landing | 偽 | 57251〜57302（ask_opened） | 15422〜15473 | 偽 | — | 真 | 1969 | 偽 | 真 |
| 127 | 11-08T01:48:29 | 4070 | 121ede52 | awaiting_answer | 1544（approve_landing） | 6.9 | answer:approve_landing | 偽 | 58346〜58381（ask_opened） | 14405〜14440 | 偽 | — | 真 | 8 | 偽 | 偽 |
| 128 | 11-08T01:59:28 | 4443 | 85bde064 | awaiting_answer | 1542（approve_landing） | 5.8 | answer:approve_landing | 偽 | 59930〜60076（ask_opened） | 12949〜13094 | 偽 | — | 真 | 4 | 偽 | 偽 |
| 129 | 11-08T02:10:40 | 4082 | 7b5a0994 | awaiting_answer | 1546（approve_landing） | 9.6 | answer:approve_landing | 偽 | 50381〜50388（ask_opened） | 14034〜14041 | 偽 | — | 真 | 1706 | 偽 | 真 |
| 130 | 11-08T02:25:16 | 4305 | 8d1a61e7 | awaiting_answer | 1548（approve_landing） | 9.5 | answer:approve_landing | 偽 | 42492〜42665（ask_opened） | 13208〜13381 | 偽 | — | 真 | 2926 | 偽 | 真 |
| 131 | 11-08T03:14:24 | 4222 | 415d51ff | disk_space | — | 6.1 | landing_slot | 偽 | 1841〜2012（turn_finished） | 1251〜1423 | 偽 | — | 偽 | 無し | — | — |
| 132 | 11-08T04:52:34 | 4224 | 9948478a | disk_space | — | 6.0 | landing_slot | 偽 | 1559〜1782（turn_finished） | 595〜818 | 偽 | — | 偽 | 無し | — | — |
| 133 | 11-08T05:24:57 | 4361 | 332fec87 | awaiting_answer | 1547（approve_landing） | 6.9 | answer:approve_landing | 偽 | 53555〜53557（ask_opened） | 2393〜2395 | 偽 | — | 真 | 997 | 偽 | 偽 |
| 134 | 11-08T06:04:56 | 4452 | 12e3bfa9 | awaiting_answer | 1551（approve_landing） | 9.4 | answer:approve_landing | 偽 | 0〜3（ask_opened） | 417〜421 | 偽 | — | 真 | 3205 | 偽 | 真 |
| 135 | 11-08T06:22:46 | 4093 | eee35312 | awaiting_answer | 1552（approve_landing） | 12.7 | answer:approve_landing | 偽 | 0〜24（ask_opened） | 2706〜2730 | 偽 | — | 真 | 1029 | 偽 | 真 |
| 136 | 11-08T07:14:47 | 4443 | 85bde064 | awaiting_answer | 1560（approve_landing） | 8.8 | answer:approve_landing | 偽 | 0〜14（ask_opened） | 64〜78 | 真 | 偽 | 真 | 2457 | 偽 | 真 |
| 137 | 11-08T07:18:50 | 4452 | 12e3bfa9 | awaiting_answer | 1561（approve_landing） | 12.2 | answer:approve_landing | 偽 | 0〜15（ask_opened） | 82〜97 | 真 | 偽 | 偽 | 無し | — | — |
| 138 | 11-08T08:04:25 | 4443 | 85bde064 | awaiting_answer | 1563（approve_landing） | 8.7 | answer:approve_landing | 偽 | 0〜39（ask_opened） | 814〜853 | 真 | 偽 | 偽 | 無し | — | — |
| 139 | 11-08T08:42:39 | 4452 | 15c0197b | disk_space | — | 7.6 | landing_slot | 偽 | 188〜665（turn_finished） | 205〜682 | 真 | 偽 | 偽 | 無し | — | — |
| 140 | 11-08T09:27:34 | 4471 | eb140f9e | disk_space | — | 9.7 | landing_slot | 偽 | 75〜89（turn_finished） | 1150〜1165 | 真 | 偽 | 偽 | 無し | — | — |
| 141 | 11-08T10:13:49 | 4569 | dc488f4f | awaiting_answer | 1567（approve_landing） | 12.5 | answer:approve_landing | 偽 | 0〜20（ask_opened） | 129〜148 | 真 | 偽 | 偽 | 無し | — | — |
| 142 | 11-08T10:34:40 | 4093 | eee35312 | disk_space | — | 7.4 | needs_session_other | 偽 | 198〜224（turn_finished） | 5304〜5329 | 真 | 偽 | 真 | 1278 | 偽 | 真 |
| 143 | 11-08T10:46:44 | 4093 | eee35312 | disk_space | — | 4.9 | needs_session_other | 偽 | 198〜948（turn_finished） | 4580〜5329 | 真 | 偽 | 真 | 1278 | 偽 | 真 |
| 144 | 11-08T10:46:44 | 4472 | 4adea518 | disk_space | — | 7.1 | landing_slot | 偽 | 664〜1160（turn_finished） | 1473〜1968 | 真 | 偽 | 偽 | 無し | — | — |
| 145 | 11-08T11:23:18 | 4470 | 84a449ef | disk_space | — | 7.3 | landing_slot | 偽 | 1207〜1917（turn_finished） | 1294〜2004 | 真 | 偽 | 偽 | 無し | — | — |
| 146 | 11-08T11:59:26 | 4484 | 47e99fb6 | disk_space | — | 10.4 | landing_slot | 偽 | 78〜124（turn_finished） | 754〜800 | 真 | 真 | 偽 | 無し | — | — |
| 147 | 11-08T12:27:04 | 4093 | eee35312 | awaiting_answer | 1569（approve_landing） | 10.9 | answer:approve_landing | 偽 | 0〜13（ask_opened） | 316〜328 | 偽 | — | 真 | 1968 | 偽 | 真 |
| 148 | 11-08T14:39:11 | 4448 | 582ad032 | disk_space | — | 1.9 | landing_slot | 偽 | 177〜584（turn_finished） | 62〜469 | 偽 | — | 偽 | 無し | — | — |
| 149 | 11-08T15:32:06 | 4473 | f701612f | disk_space | — | 13.1 | landing_slot | 偽 | 238〜299（turn_finished） | 5〜66 | 真 | 偽 | 偽 | 無し | — | — |
| 150 | 11-08T15:44:14 | 4589 | a0a8d2cd | disk_space | — | 9.7 | landing_slot | 偽 | 106〜171（turn_finished） | 526〜592 | 真 | 偽 | 偽 | 無し | — | — |
| 151 | 11-08T16:32:11 | 4093 | eee35312 | awaiting_answer | 1573（approve_landing） | 10.6 | answer:approve_landing | 偽 | 0〜33（ask_opened） | 9319〜9351 | 真 | 偽 | 真 | 2111 | 偽 | 真 |
| 152 | 11-08T16:52:35 | 4182 | e8b828a3 | disk_space | — | 8.1 | landing_slot | 偽 | 165〜223（turn_finished） | 304〜362 | 真 | 真 | 偽 | 無し | — | — |
| 153 | 11-08T16:57:40 | 4059 | fcf6b099 | awaiting_answer | 1574（approve_landing） | 16.3 | answer:approve_landing | 偽 | 0〜6（ask_opened） | 20611〜20618 | 真 | 真 | 真 | 2494 | 偽 | 真 |
| 154 | 11-08T17:03:31 | 4082 | 7b5a0994 | awaiting_answer | 1576（approve_landing） | 10.3 | answer:approve_landing | 偽 | 0〜8（ask_opened） | 20396〜20405 | 偽 | — | 真 | 1293 | 偽 | 偽 |
| 155 | 11-08T17:51:13 | 4305 | 8d1a61e7 | disk_space | — | 7.1 | landing_slot | 偽 | 219〜287（turn_finished） | 1〜70 | 真 | 真 | 偽 | 無し | — | — |
| 156 | 11-08T17:58:49 | 4305 | 8d1a61e7 | disk_space | — | 2.7 | landing_slot | 偽 | 670〜743（turn_finished） | 1〜74 | 真 | 真 | 偽 | 無し | — | — |
| 157 | 11-08T18:00:27 | 4474 | acb0f2f0 | awaiting_answer | 1577（approve_landing） | 9.8 | answer:approve_landing | 偽 | 0〜21（ask_opened） | 17033〜17054 | 偽 | — | 真 | 1780 | 偽 | 偽 |
| 158 | 11-08T18:07:29 | 4597 | eaa8ebf5 | disk_space | — | 8.9 | landing_slot | 偽 | 100〜106（turn_finished） | 827〜833 | 偽 | — | 偽 | 1536 | 偽 | 真 |
| 159 | 11-08T18:50:06 | 4597 | eaa8ebf5 | disk_space | — | 8.9 | landing_slot | 真 | 139〜170（turn_finished） | 0〜30 | 真 | 真 | 偽 | 無し | — | — |
| 160 | 11-08T19:15:19 | 4590 | 6d7ecfdc | awaiting_answer | 1578（approve_landing） | 11.7 | answer:approve_landing | 偽 | 0〜20（ask_opened） | 12590〜12611 | 真 | 真 | 偽 | 866 | 偽 | 真 |
| 161 | 11-08T19:27:35 | 4542 | b9845a34 | awaiting_answer | 1579（approve_landing） | 13.0 | answer:approve_landing | 偽 | 0〜9（ask_opened） | 11903〜11911 | 真 | 偽 | 真 | 987 | 偽 | 真 |
| 162 | 11-08T19:43:04 | 4093 | eee35312 | awaiting_answer | 1573（approve_landing） | 8.2 | answer:approve_landing | 偽 | 11475〜11485（ask_opened） | 10614〜10625 | 不明 | 真 | 真 | 1617 | 偽 | 真 |
| 163 | 11-08T19:51:43 | 4043 | 10577b29 | awaiting_answer | 1580（approve_landing） | 11.0 | answer:approve_landing | 偽 | 0〜8（ask_opened） | 10569〜10577 | 不明 | 不明 | 偽 | 無し | — | — |
| 164 | 11-08T20:11:52 | 4361 | 332fec87 | disk_space | — | 7.6 | landing_slot | 偽 | 187〜504（turn_finished） | 2〜319 | 真 | 真 | 偽 | 無し | — | — |
| 165 | 11-08T20:15:18 | 4361 | 332fec87 | disk_space | — | 2.7 | landing_slot | 偽 | 704〜709（turn_finished） | 1〜6 | 偽 | — | 偽 | 無し | — | — |
| 166 | 11-08T20:50:20 | 4591 | 69ead960 | disk_space | — | 15.4 | landing_slot | 偽 | 277〜299（turn_finished） | 105〜126 | 真 | 真 | 偽 | 無し | — | — |
| 167 | 11-08T21:34:17 | 4598 | 444e1d7d | disk_space | — | 11.0 | landing_slot | 偽 | 327〜477（turn_finished） | 541〜690 | 真 | 偽 | 偽 | 987 | 偽 | 真 |
| 168 | 11-08T22:08:57 | 4106 | 6d35c1d2 | disk_space | — | 14.1 | landing_slot | 偽 | 311〜363（turn_finished） | 64〜115 | 真 | 真 | 偽 | 無し | — | — |
| 169 | 11-08T22:09:59 | 4598 | 444e1d7d | disk_space | — | 12.1 | landing_slot | 偽 | 381〜396（turn_finished） | 2057〜2072 | 真 | 真 | 偽 | 無し | — | — |
| 170 | 11-08T22:13:14 | 4071 | f05833b1 | awaiting_answer | 1581（approve_landing） | 13.6 | answer:approve_landing | 偽 | 0〜7（ask_opened） | 2144〜2151 | 不明 | 不明 | 真 | 2081 | 偽 | 真 |
| 171 | 11-08T23:07:13 | 4362 | 17f845dc | disk_space | — | 6.0 | landing_slot | 偽 | 126〜1493（turn_finished） | 1241〜2607 | 不明 | 偽 | 偽 | 無し | — | — |
| 172 | 11-08T23:37:17 | 4309 | b69cb2cb | awaiting_answer | 1585（approve_landing） | 11.7 | answer:approve_landing | 偽 | 0〜4（ask_opened） | 269〜273 | 真 | 真 | 真 | 1082 | 偽 | 偽 |
| 173 | 11-08T23:44:42 | 4059 | fcf6b099 | awaiting_answer | 1586（approve_landing） | 11.7 | answer:approve_landing | 偽 | 0〜7（ask_opened） | 535〜542 | 不明 | 不明 | 真 | 1675 | 偽 | 偽 |
| 174 | 11-08T23:59:27 | 4474 | acb0f2f0 | disk_space | — | 11.5 | landing_slot | 偽 | 181〜328（turn_finished） | 531〜678 | 不明 | 真 | 偽 | 無し | — | — |
| 175 | 11-09T00:04:42 | 4542 | b9845a34 | awaiting_answer | 1588（approve_landing） | 6.0 | answer:approve_landing | 偽 | 0〜6（ask_opened） | 95〜100 | 真 | 真 | 真 | 785 | 偽 | 真 |
| 176 | 11-09T00:26:38 | 4542 | b9845a34 | disk_space | — | 4.7 | landing_slot | 真 | 303〜306（turn_finished） | 0〜3 | 偽 | — | 偽 | 無し | — | — |
| 177 | 11-09T00:31:54 | 4542 | b9845a34 | disk_space | — | 2.7 | landing_slot | 偽 | 615〜623（turn_finished） | 215〜223 | 偽 | — | 偽 | 無し | — | — |
| 178 | 11-09T01:06:10 | 4590 | 6d7ecfdc | disk_space | — | 6.0 | landing_slot | 偽 | 3〜1526（turn_finished） | 1237〜2759 | 不明 | 不明 | 偽 | 無し | — | — |
| 179 | 11-09T01:32:16 | 4093 | eee35312 | awaiting_answer | 1592（approve_landing） | 10.8 | answer:approve_landing | 偽 | 0〜6（ask_opened） | 6270〜6276 | 真 | 真 | 偽 | 無し | — | — |
| 180 | 11-09T02:39:56 | 4071 | f05833b1 | disk_space | — | 7.3 | landing_slot | 偽 | 189〜228（turn_finished） | 1〜40 | 真 | 偽 | 偽 | 412 | 偽 | 真 |
| 181 | 11-09T02:59:38 | 4071 | f05833b1 | awaiting_answer | 1593（approve_landing） | 11.6 | answer:approve_landing | 偽 | 0〜3（ask_opened） | 1172〜1175 | 真 | 偽 | 偽 | 1745 | 偽 | 真 |
| 182 | 11-09T03:21:42 | 4093 | eee35312 | disk_space | — | 2.7 | landing_slot | 偽 | 6852〜6898（turn_finished） | 1〜48 | 真 | 偽 | 偽 | 無し | — | — |
| 183 | 11-09T03:25:44 | 4616 | c1b93a46 | awaiting_answer | 1595（approve_landing） | 12.0 | answer:approve_landing | 偽 | 0〜9（ask_opened） | 66〜75 | 真 | 偽 | 真 | 1288 | 偽 | 真 |
| 184 | 11-09T03:34:13 | 4309 | b69cb2cb | disk_space | — | 7.9 | landing_slot | 偽 | 165〜230（turn_finished） | 6315〜6380 | 真 | 偽 | 偽 | 無し | — | — |
| 185 | 11-09T03:53:06 | 4616 | c1b93a46 | awaiting_answer | 1599（approve_landing） | 8.4 | answer:approve_landing | 偽 | 0〜4（ask_opened） | 1176〜1181 | 真 | 偽 | 真 | 821 | 偽 | 真 |
| 186 | 11-09T04:30:10 | 4616 | c1b93a46 | disk_space | — | 8.5 | landing_slot | 偽 | 207〜223（turn_finished） | 5447〜5463 | 真 | 偽 | 偽 | 無し | — | — |
| 187 | 11-09T05:48:59 | 4071 | f05833b1 | disk_space | — | 7.6 | landing_slot | 偽 | 4〜22（turn_finished） | 1999〜2017 | 真 | 偽 | 偽 | 無し | — | — |
| 188 | 11-09T06:38:03 | 4073 | 8d8389e0 | awaiting_answer | 1604（approve_landing） | 9.6 | answer:approve_landing | 偽 | 0〜7（ask_opened） | 55〜62 | 不明 | 不明 | 真 | 759 | 偽 | 真 |
| 189 | 11-09T06:55:05 | 4073 | 8d8389e0 | disk_space | — | 5.9 | landing_slot | 偽 | 153〜202（turn_finished） | 4〜54 | 真 | 偽 | 偽 | 無し | — | — |
| 190 | 11-09T07:11:54 | 4639 | e0dfd14e | disk_space | — | 7.2 | landing_slot | 偽 | 129〜192（turn_finished） | 335〜399 | 真 | 偽 | 偽 | 988 | 偽 | 真 |
| 191 | 11-09T07:42:18 | 4639 | e0dfd14e | disk_space | — | 7.6 | needs_session_other | 真 | 1988〜2011（turn_finished） | 0〜23 | 不明 | 不明 | 真 | 988 | 偽 | 真 |
| 192 | 11-09T07:59:14 | 4617 | d6197161 | disk_space | — | 10.0 | landing_slot | 偽 | 100〜138（turn_finished） | 1〜39 | 真 | 偽 | 偽 | 無し | — | — |
| 193 | 11-09T08:00:21 | 4639 | e0dfd14e | disk_space | — | 8.4 | landing_slot | 偽 | 68〜95（turn_finished） | 580〜607 | 真 | 偽 | 偽 | 無し | — | — |
| 194 | 11-09T08:15:53 | 4072 | fb33c0f3 | disk_space | — | 7.7 | landing_slot | 偽 | 209〜250（turn_finished） | 1686〜1726 | 真 | 偽 | 偽 | 無し | — | — |
| 195 | 11-09T09:44:09 | 4318 | 660f2424 | awaiting_answer | 1614（approve_landing） | 12.4 | answer:approve_landing | 偽 | 0〜6（ask_opened） | 166〜172 | 不明 | 不明 | 真 | 732 | 偽 | 真 |
| 196 | 11-09T10:03:30 | 4318 | 660f2424 | disk_space | — | 7.0 | landing_slot | 真 | 221〜256（turn_finished） | 0〜35 | 真 | 偽 | 偽 | 176 | 偽 | 真 |
| 197 | 11-09T10:34:03 | 4135 | 2c86f6dd | disk_space | — | 6.8 | landing_slot | 偽 | 168〜212（turn_finished） | 461〜505 | 真 | 偽 | 偽 | 無し | — | — |
| 198 | 11-09T10:41:45 | 4318 | 660f2424 | disk_space | — | 10.5 | landing_slot | 偽 | 208〜234（turn_finished） | 551〜576 | 真 | 真 | 偽 | 無し | — | — |
| 199 | 11-09T10:44:29 | 4327 | 393e252a | awaiting_answer | 1618（approve_landing） | 16.2 | answer:approve_landing | 偽 | 0〜4（ask_opened） | 1081〜1084 | 真 | 真 | 偽 | 1157 | 偽 | 偽 |
| 200 | 11-09T11:02:32 | 4644 | 3356a064 | disk_space | — | 1.4 | landing_slot | 偽 | 113〜165（turn_finished） | 1476〜1528 | 真 | 偽 | 偽 | 無し | — | — |
| 201 | 11-09T12:44:24 | 4658 | 3b514575 | disk_space | — | 13.2 | landing_slot | 偽 | 190〜238（turn_finished） | 1〜49 | 真 | 偽 | 偽 | 無し | — | — |
| 202 | 11-09T13:37:00 | 4662 | d7116e8d | awaiting_answer | 1621（approve_landing） | 13.7 | answer:approve_landing | 偽 | 0〜9（ask_opened） | 2656〜2665 | 不明 | 不明 | 真 | 111 | 偽 | 偽 |
| 203 | 11-09T14:41:32 | 4662 | d7116e8d | disk_space | — | 5.9 | landing_slot | 偽 | 206〜249（turn_finished） | 160〜203 | 真 | 偽 | 偽 | 無し | — | — |
| 204 | 11-09T14:44:13 | 4670 | d6ad7496 | awaiting_answer | 1623（approve_landing） | 12.3 | answer:approve_landing | 偽 | 0〜4（ask_opened） | 133〜136 | 真 | 偽 | 真 | 1320 | 偽 | 真 |
| 205 | 11-09T15:11:39 | 4670 | d6ad7496 | disk_space | — | 8.6 | landing_slot | 偽 | 149〜188（turn_finished） | 1379〜1419 | 真 | 真 | 偽 | 無し | — | — |
| 206 | 11-09T16:25:31 | 4676 | d1f32ac8 | disk_space | — | 11.4 | landing_slot | 偽 | 216〜225（turn_finished） | 2〜12 | 真 | 偽 | 偽 | 無し | — | — |
| 207 | 11-09T17:37:29 | 4654 | e738212b | disk_space | — | 12.5 | landing_slot | 真 | 118〜183（turn_finished） | 0〜65 | 真 | 偽 | 偽 | 無し | — | — |
| 208 | 11-09T17:37:29 | 4653 | 54310e37 | disk_space | — | 15.2 | landing_slot | 偽 | 203〜218（turn_finished） | 805〜820 | 真 | 偽 | 偽 | 1672 | 偽 | 偽 |
| 209 | 11-09T17:47:34 | 4660 | f080cad7 | disk_space | 1624（worker_question） | 20.2 | answer:worker_question | 偽 | 1〜49（ask_opened） | 15720〜15768 | 真 | 真 | 真 | 512 | 偽 | 真 |
| 210 | 11-09T18:40:06 | 4653 | 54310e37 | awaiting_answer | 1625（approve_landing） | 11.5 | answer:approve_landing | 偽 | 0〜8（ask_opened） | 12625〜12634 | 不明 | 真 | 真 | 1808 | 偽 | 真 |
| 211 | 11-09T18:59:17 | 4655 | ae9142f4 | awaiting_answer | 1626（approve_landing） | 12.4 | answer:approve_landing | 偽 | 0〜8（ask_opened） | 11510〜11518 | 不明 | 真 | 真 | 1097 | 偽 | 偽 |
| 212 | 11-09T19:08:44 | 4071 | d46e8104 | awaiting_answer | 1627（approve_landing） | 9.9 | answer:approve_landing | 偽 | 0〜7（ask_opened） | 5852〜5859 | 不明 | 不明 | 真 | 1309 | 偽 | 真 |
| 213 | 11-09T19:59:49 | 4093 | 8ed92987 | disk_space | — | 13.5 | landing_slot | 偽 | 283〜300（turn_finished） | 3〜20 | 真 | 偽 | 偽 | 無し | — | — |
| 214 | 11-09T20:06:37 | 4319 | 98918baf | disk_space | — | 14.0 | landing_slot | 偽 | 100〜130（turn_finished） | 765〜795 | 真 | 偽 | 偽 | 無し | — | — |
| 215 | 11-09T21:06:42 | 4261 | 503bd31d | disk_space | — | 10.7 | landing_slot | 偽 | 212〜243（turn_finished） | 2〜33 | 真 | 偽 | 偽 | 無し | — | — |
| 216 | 11-09T21:09:37 | 4071 | d46e8104 | awaiting_answer | 1627（approve_landing） | 8.6 | answer:approve_landing | 偽 | 7256〜7260（ask_opened） | 3725〜3729 | 真 | 偽 | 真 | 1314 | 偽 | 真 |
| 217 | 11-09T21:28:45 | 4332 | 4dae84e4 | awaiting_answer | 1629（approve_landing） | 12.3 | answer:approve_landing | 偽 | 0〜5（ask_opened） | 2665〜2671 | 不明 | 不明 | 真 | 3009 | 偽 | 真 |
| 218 | 11-09T21:55:56 | 4259 | 236156c5 | awaiting_answer | 1630（approve_landing） | 12.5 | answer:approve_landing | 偽 | 0〜4（ask_opened） | 1076〜1079 | 真 | 真 | 偽 | 無し | — | — |
| 219 | 11-09T22:14:46 | 4145 | 148646f6 | disk_space | — | 9.6 | landing_slot | 偽 | 91〜99（turn_finished） | 2〜9 | 真 | 偽 | 偽 | 無し | — | — |
| 220 | 11-09T22:21:22 | 4660 | f080cad7 | awaiting_answer | 1631（approve_landing） | 4.8 | answer:approve_landing | 偽 | 0〜2（ask_opened） | 129〜131 | 真 | 偽 | 真 | 421 | 偽 | 偽 |
| 221 | 11-09T22:34:20 | 4298 | ce6f5010 | disk_space | — | 16.6 | landing_slot | 偽 | 374〜669（turn_finished） | 1158〜1453 | 真 | 偽 | 偽 | 無し | — | — |
| 222 | 11-09T22:57:21 | 4653 | 54310e37 | disk_space | — | 9.5 | landing_slot | 偽 | 337〜346（turn_finished） | 127〜136 | 真 | 偽 | 偽 | 無し | — | — |
| 223 | 11-09T23:59:10 | 4655 | ae9142f4 | disk_space | — | 10.6 | landing_slot | 偽 | 347〜381（turn_finished） | 790〜823 | 真 | 偽 | 偽 | 無し | — | — |
| 224 | 11-10T00:25:49 | 4660 | f080cad7 | awaiting_answer | 1636（approve_landing） | 12.5 | answer:approve_landing | 偽 | 0〜5（ask_opened） | 55〜60 | 真 | 偽 | 真 | 1864 | 偽 | 真 |
| 225 | 11-10T00:26:06 | 4071 | d46e8104 | disk_space | — | 7.4 | landing_slot | 偽 | 242〜281（turn_finished） | 449〜488 | 真 | 偽 | 偽 | 無し | — | — |
| 226 | 11-10T01:10:19 | 4660 | f080cad7 | awaiting_answer | 1637（approve_landing） | 6.7 | answer:approve_landing | 偽 | 0〜11（ask_opened） | 45〜56 | 真 | 真 | 真 | 71 | 偽 | 偽 |
| 227 | 11-10T01:29:20 | 4332 | 4dae84e4 | awaiting_answer | 1641（approve_landing） | 11.0 | answer:approve_landing | 偽 | 0〜3（ask_opened） | 9455〜9458 | 真 | 真 | 真 | 1710 | 偽 | 真 |
| 228 | 11-10T02:00:48 | 4074 | 2e68ab2a | awaiting_answer | 1642（approve_landing） | 9.2 | answer:approve_landing | 偽 | 0〜12（ask_opened） | 9294〜9305 | 不明 | 不明 | 真 | 788 | 偽 | 真 |
| 229 | 11-10T02:34:09 | 4260 | 7c82eabb | disk_space | — | 9.5 | landing_slot | 偽 | 104〜168（turn_finished） | 1〜66 | 真 | 真 | 偽 | 無し | — | — |
| 230 | 11-10T02:36:23 | 4170 | 0e4365a1 | awaiting_answer | 1643（approve_landing） | 16.2 | answer:approve_landing | 偽 | 0〜6（ask_opened） | 15882〜15888 | 真 | 真 | 真 | 2134 | 偽 | 偽 |
| 231 | 11-10T03:45:17 | 4299 | 8e7cbe3c | disk_space | — | 13.2 | landing_slot | 真 | 109〜122（turn_finished） | 0〜13 | 真 | 偽 | 偽 | 無し | — | — |
| 232 | 11-10T03:56:00 | 4334 | ba344bd2 | awaiting_answer | 1644（approve_landing） | 14.2 | answer:approve_landing | 偽 | 0〜8（ask_opened） | 3187〜3195 | 真 | 真 | 真 | 1185 | 偽 | 真 |
| 233 | 11-10T04:35:44 | 4332 | 4dae84e4 | awaiting_answer | 1641（approve_landing） | 8.0 | answer:approve_landing | 偽 | 11182〜11187（ask_opened） | 8566〜8571 | 真 | 偽 | 真 | 1989 | 偽 | 偽 |
| 234 | 11-10T04:37:48 | 3949 | 8d071db1 | disk_space | — | 10.8 | landing_slot | 偽 | 146〜172（turn_finished） | 3〜30 | 真 | 偽 | 偽 | 無し | — | — |
| 235 | 11-10T04:49:10 | 4074 | 2e68ab2a | awaiting_answer | 1642（approve_landing） | 6.6 | answer:approve_landing | 偽 | 10106〜10114（ask_opened） | 7861〜7868 | 真 | 偽 | 真 | 8521 | 偽 | 偽 |
| 236 | 11-10T05:09:14 | 4334 | ba344bd2 | awaiting_answer | 1644（approve_landing） | 8.7 | answer:approve_landing | 偽 | 4396〜4402（ask_opened） | 6780〜6786 | 真 | 真 | 真 | 1851 | 偽 | 真 |
| 237 | 11-10T05:52:16 | 4175 | 59a4839d | awaiting_answer | 1646（approve_landing） | 17.2 | answer:approve_landing | 偽 | 0〜11（ask_opened） | 4336〜4346 | 不明 | 不明 | 真 | 5353 | 偽 | 真 |
| 238 | 11-10T06:02:13 | 4385 | ca04c44c | disk_space | — | 15.8 | landing_slot | 偽 | 311〜334（turn_finished） | 3〜25 | 真 | 偽 | 偽 | 2116 | 偽 | 真 |
| 239 | 11-10T06:11:03 | 3950 | 7aff0122 | disk_space | — | 6.7 | landing_slot | 偽 | 148〜191（turn_finished） | 234〜277 | 真 | 偽 | 偽 | 無し | — | — |
| 240 | 11-10T07:44:42 | 4385 | ca04c44c | disk_space | — | 15.5 | landing_answer_no_ask | 偽 | 3204〜3266（turn_finished） | 5656〜5719 | 真 | 偽 | 偽 | 無し | — | — |
| 241 | 11-10T10:04:43 | 4385 | ca04c44c | disk_space | — | 1.4 | landing_slot | 偽 | 11603〜11667（turn_finished） | 12055〜12119 | 真 | 偽 | 偽 | 無し | — | — |
| 242 | 11-10T10:10:03 | 3951 | 987c4a20 | disk_space | — | 15.8 | landing_slot | 偽 | 235〜260（turn_finished） | 1〜26 | 真 | 偽 | 偽 | 無し | — | — |
| 243 | 11-10T10:21:59 | 4332 | 4dae84e4 | awaiting_answer | 1647（approve_landing） | 12.5 | answer:approve_landing | 偽 | 0〜7（ask_opened） | 2145〜2151 | 真 | 偽 | 偽 | 無し | — | — |
| 244 | 11-10T10:34:28 | 4074 | 2e68ab2a | disk_space | — | 6.6 | landing_slot | 偽 | 443〜1081（turn_finished） | 3038〜3676 | 真 | 偽 | 偽 | 4765 | 偽 | 真 |
| 245 | 11-10T11:31:27 | 4170 | 0e4365a1 | disk_space | — | 11.6 | landing_slot | 偽 | 1202〜1258（turn_finished） | 1536〜1592 | 真 | 偽 | 偽 | 無し | — | — |
| 246 | 11-10T11:57:24 | 4074 | 2e68ab2a | disk_space | — | 7.9 | needs_session_other | 真 | 6031〜6055（turn_finished） | 0〜24 | 真 | 偽 | 真 | 4765 | 偽 | 真 |
| 247 | 11-10T12:13:41 | 4334 | ba344bd2 | awaiting_answer | 1649（approve_landing） | 11.3 | answer:approve_landing | 偽 | 0〜13（ask_opened） | 52〜65 | 真 | 偽 | 真 | 1875 | 偽 | 真 |
| 248 | 11-10T13:32:46 | 4074 | 2e68ab2a | disk_space | — | 5.8 | landing_slot | 偽 | 887〜957（turn_finished） | 3866〜3936 | 真 | 偽 | 偽 | 1257 | 偽 | 真 |
| 249 | 11-10T13:50:41 | 4175 | 59a4839d | awaiting_answer | 1651（approve_landing） | 11.5 | answer:approve_landing | 偽 | 0〜15（ask_opened） | 263〜278 | 真 | 偽 | 真 | 4041 | 真 | 真 |
| 250 | 11-10T14:27:26 | 4334 | ba344bd2 | awaiting_answer | 1652（approve_landing） | 8.3 | answer:approve_landing | 偽 | 0〜8（ask_opened） | 115〜122 | 真 | 偽 | 偽 | 無し | — | — |
| 251 | 11-10T15:06:06 | 4074 | 2e68ab2a | disk_space | — | 6.5 | landing_slot | 偽 | 425〜449（turn_finished） | 1023〜1047 | 真 | 偽 | 偽 | 無し | — | — |
| 252 | 11-10T15:43:55 | 4175 | 59a4839d | awaiting_answer | 1654（approve_landing） | 13.2 | answer:approve_landing | 偽 | 0〜12（ask_opened） | 33365〜33378 | 真 | 真 | 真 | 2728 | 偽 | 偽 |
| 253 | 11-10T16:46:08 | 4688 | bc8b38c3 | disk_space | — | 16.0 | landing_slot | 真 | 382〜409（turn_finished） | 0〜27 | 真 | 偽 | 偽 | 無し | — | — |
| 254 | 11-10T17:52:58 | 4678 | 35ce9eaf | awaiting_answer | 1656（approve_landing） | 14.6 | answer:approve_landing | 偽 | 0〜9（ask_opened） | 25776〜25785 | 不明 | 真 | 真 | 806 | 偽 | 真 |
| 255 | 11-10T18:25:09 | 4646 | 5a609937 | awaiting_answer | 1657（approve_landing） | 13.9 | answer:approve_landing | 偽 | 0〜5（ask_opened） | 23881〜23886 | 不明 | 不明 | 真 | 2836 | 偽 | 偽 |
| 256 | 11-10T20:10:48 | 4727 | 32ae149b | awaiting_answer | 1658（approve_landing） | 18.1 | answer:approve_landing | 偽 | 0〜4（ask_opened） | 16922〜16926 | 真 | 真 | 真 | 1496 | 偽 | 真 |
| 257 | 11-10T21:44:24 | 4312 | ec42d99d | awaiting_answer | 1659（approve_landing） | 19.8 | answer:approve_landing | 偽 | 0〜5（ask_opened） | 4350〜4355 | 真 | 真 | 真 | 2188 | 偽 | 真 |
| 258 | 11-10T22:37:40 | 4332 | 8dc7ef8a | awaiting_answer | 1660（approve_landing） | 12.3 | answer:approve_landing | 偽 | 0〜4（ask_opened） | 8895〜8899 | 真 | 真 | 真 | 1335 | 偽 | 真 |
| 259 | 11-10T23:30:10 | 4139 | 01f8d67e | awaiting_answer | 1661（approve_landing） | 9.6 | answer:approve_landing | 偽 | 0〜4（ask_opened） | 5775〜5779 | 真 | 真 | 真 | 1927 | 偽 | 真 |
| 260 | 11-10T23:33:48 | 4312 | ec42d99d | awaiting_answer | 1659（approve_landing） | 11.6 | answer:approve_landing | 偽 | 6558〜6569（ask_opened） | 5502〜5513 | 不明 | 不明 | 真 | 1133 | 偽 | 偽 |
| 261 | 11-10T23:55:18 | 4379 | 36db323c | awaiting_answer | 1662（approve_landing） | 16.3 | answer:approve_landing | 偽 | 0〜5（ask_opened） | 4351〜4357 | 真 | 真 | 真 | 1598 | 偽 | 偽 |
| 262 | 11-11T00:03:11 | 4334 | 66aae846 | disk_space | — | 8.8 | landing_slot | 偽 | 246〜252（turn_finished） | 2〜8 | 真 | 偽 | 偽 | 無し | — | — |
| 263 | 11-11T00:33:53 | 4541 | e0994552 | disk_space | — | 7.4 | landing_slot | 偽 | 116〜179（turn_finished） | 274〜336 | 真 | 真 | 偽 | 無し | — | — |
| 264 | 11-11T01:24:30 | 4727 | 32ae149b | disk_space | — | 7.0 | unknown:landing_answer_no_ask|landing_slot | 偽 | — | — | 不明 | 偽 | 不明 | 1293 | 偽 | 偽 |
| 265 | 11-11T01:40:29 | 4678 | 35ce9eaf | disk_space | — | 6.3 | landing_slot | 偽 | 168〜216（turn_finished） | 69〜118 | 真 | 偽 | 偽 | 無し | — | — |
| 266 | 11-11T01:41:35 | 3954 | a1bfa32f | disk_space | — | 9.5 | landing_slot | 偽 | 250〜274（turn_finished） | 991〜1015 | 真 | 偽 | 偽 | 無し | — | — |
| 267 | 11-11T03:19:21 | 4175 | 59a4839d | disk_space | — | 12.5 | needs_session_other | 偽 | 3〜42（turn_finished） | ≥ 484996（未終了） | 真 | 偽 | 不明 | 不明 | 不明 | 不明 |
| 268 | 11-11T03:39:11 | 4629 | 5c4b4efa | awaiting_answer | 1671（approve_landing） | 21.6 | answer:approve_landing | 偽 | 0〜9（ask_opened） | 57〜65 | 真 | 偽 | 真 | 1862 | 偽 | 真 |
| 269 | 11-11T04:17:59 | 4646 | 5a609937 | awaiting_answer | 1672（approve_landing） | 15.2 | answer:approve_landing | 偽 | 0〜6（ask_opened） | 4709〜4716 | 真 | 偽 | 真 | 2837 | 偽 | 偽 |
| 270 | 11-11T04:29:31 | 4379 | 36db323c | disk_space | — | 12.5 | landing_slot | 偽 | 230〜281（turn_finished） | 2〜53 | 真 | 偽 | 偽 | 無し | — | — |
| 271 | 11-11T04:39:10 | 4312 | ec42d99d | disk_space | — | 10.9 | resume_after_limit | 偽 | 3〜50（turn_finished） | 1859〜1906 | 真 | 偽 | 真 | 1110 | 偽 | 真 |
| 272 | 11-11T04:55:25 | 4139 | 01f8d67e | disk_space | — | 10.1 | landing_slot | 偽 | 266〜321（turn_finished） | 888〜943 | 真 | 偽 | 偽 | 884 | 偽 | 真 |
| 273 | 11-11T05:10:11 | 4629 | 5c4b4efa | awaiting_answer | 1673（approve_landing） | 10.8 | answer:approve_landing | 偽 | 0〜7（ask_opened） | 1622〜1629 | 真 | 偽 | 真 | 1808 | 偽 | 偽 |
| 274 | 11-11T05:36:33 | 4312 | ec42d99d | awaiting_answer | 1674（approve_landing） | 7.7 | answer:approve_landing | 偽 | 0〜4（ask_opened） | 232〜236 | 真 | 偽 | 真 | 2724 | 偽 | 偽 |
| 275 | 11-11T05:47:00 | 4139 | 01f8d67e | disk_space | — | 8.3 | landing_slot | 偽 | 129〜191（turn_finished） | 373〜435 | 真 | 偽 | 偽 | 1099 | 偽 | 真 |
| 276 | 11-11T06:33:53 | 4139 | 01f8d67e | disk_space | — | 8.1 | needs_session_other | 偽 | 2954〜3003（turn_finished） | 3374〜3424 | 真 | 偽 | 真 | 1099 | 偽 | 真 |
| 277 | 11-11T06:58:09 | 4332 | 8dc7ef8a | disk_space | — | 7.5 | landing_slot | 偽 | 102〜162（turn_finished） | 1922〜1981 | 真 | 偽 | 偽 | 無し | — | — |
| 278 | 11-11T07:30:09 | 4646 | 5a609937 | awaiting_answer | 1675（approve_landing） | 16.0 | answer:approve_landing | 偽 | 0〜5（ask_opened） | 1921〜1926 | 真 | 偽 | 真 | 1402 | 偽 | 真 |
| 279 | 11-11T07:53:47 | 4139 | 01f8d67e | disk_space | — | 7.4 | landing_slot | 偽 | 289〜319（turn_finished） | 31〜61 | 真 | 真 | 偽 | 無し | — | — |
| 280 | 11-11T08:05:24 | 4312 | ec42d99d | disk_space | — | 21.1 | landing_slot | 偽 | 354〜413（turn_finished） | 39〜98 | 真 | 偽 | 偽 | 無し | — | — |
| 281 | 11-11T08:18:09 | 4312 | ec42d99d | disk_space | — | 2.8 | landing_slot | 偽 | 1131〜1178（turn_finished） | 230〜277 | 真 | 偽 | 偽 | 無し | — | — |
| 282 | 11-11T08:44:09 | 4629 | 5c4b4efa | awaiting_answer | 1677（approve_landing） | 12.0 | answer:approve_landing | 偽 | 0〜8（ask_opened） | 71〜78 | 真 | 偽 | 真 | 551 | 偽 | 真 |
| 283 | 11-11T08:51:38 | 4646 | 5a609937 | disk_space | — | 9.2 | landing_slot | 偽 | 281〜297（turn_finished） | 3〜19 | 真 | 偽 | 偽 | 無し | — | — |
| 284 | 11-11T09:02:25 | 4629 | 5c4b4efa | awaiting_answer | 1678（approve_landing） | 6.2 | answer:approve_landing | 偽 | 0〜12（ask_opened） | 3984〜3995 | 真 | 真 | 偽 | 無し | — | — |
| 285 | 11-11T10:20:10 | 4754 | 63241a1a | awaiting_answer | 1680（approve_landing） | 14.1 | answer:approve_landing | 偽 | 0〜5（ask_opened） | 1147〜1152 | 真 | 真 | 偽 | 無し | — | — |
| 286 | 11-11T10:58:25 | 4665 | 1e91081b | disk_space | — | 10.0 | landing_slot | 偽 | 267〜302（turn_finished） | 173〜209 | 真 | 偽 | 偽 | 無し | — | — |
| 287 | 11-11T12:10:55 | 4755 | 9f21aeb0 | awaiting_answer | 1682（approve_landing） | 13.6 | answer:approve_landing | 偽 | 0〜9（ask_opened） | 8〜17 | 不明 | 不明 | 真 | 1463 | 偽 | 真 |
| 288 | 11-11T12:35:58 | 4755 | 9f21aeb0 | disk_space | — | 6.4 | unknown:landing_answer_no_ask|landing_slot | 偽 | — | — | 真 | 真 | 不明 | 無し | — | — |
| 289 | 11-11T12:43:02 | 4666 | e0694e86 | awaiting_answer | 1683（approve_landing） | 10.8 | answer:approve_landing | 偽 | 0〜8（ask_opened） | 1743〜1751 | 不明 | 不明 | 真 | 1275 | 偽 | 偽 |
| 290 | 11-11T12:45:22 | 4667 | 142b784f | disk_space | — | 10.0 | landing_slot | 偽 | 152〜184（turn_finished） | 997〜1029 | 真 | 偽 | 偽 | 無し | — | — |
| 291 | 11-11T12:55:39 | 4669 | 89df40f7 | disk_space | — | 6.4 | landing_slot | 偽 | 70〜110（turn_finished） | 3615〜3656 | 真 | 偽 | 偽 | 無し | — | — |
| 292 | 11-11T14:30:10 | 4666 | e0694e86 | awaiting_answer | 1685（approve_landing） | 11.0 | answer:approve_landing | 偽 | 0〜7（ask_opened） | 30449〜30455 | 真 | 真 | 真 | 1683 | 偽 | 真 |
| 293 | 11-11T15:57:54 | 4792 | 8a3f5145 | disk_space | — | 12.3 | landing_slot | 偽 | 119〜144（turn_finished） | 1〜27 | 真 | 偽 | 偽 | 無し | — | — |
| 294 | 11-11T16:33:30 | 4790 | fa619256 | awaiting_answer | 1686（approve_landing） | 15.0 | answer:approve_landing | 偽 | 0〜11（ask_opened） | 1678〜1689 | 真 | 偽 | 真 | 1739 | 偽 | 真 |
| 295 | 11-11T17:30:41 | 4791 | 371c5024 | awaiting_answer | 1688（approve_landing） | 10.3 | answer:approve_landing | 偽 | 0〜10（ask_opened） | 16913〜16922 | 不明 | 真 | 真 | 1394 | 偽 | 真 |
| 296 | 11-11T17:30:58 | 4790 | fa619256 | awaiting_answer | 1686（approve_landing） | 11.4 | answer:approve_landing | 偽 | 3451〜3458（ask_opened） | 16861〜16868 | 真 | 真 | 真 | 1602 | 偽 | 真 |
| 297 | 11-11T18:33:56 | 4677 | 0082f5e2 | disk_space | — | 16.5 | needs_session_other | 偽 | 1092〜1100（turn_finished） | 3〜11 | 真 | 真 | 真 | 909 | 偽 | 真 |
| 298 | 11-11T18:52:08 | 4677 | 0082f5e2 | disk_space | — | 6.4 | landing_slot | 偽 | 138〜176（turn_finished） | 3140〜3179 | 真 | 偽 | 偽 | 無し | — | — |
| 299 | 11-11T20:12:36 | 4729 | e3c32ad3 | disk_space | — | 7.5 | landing_slot | 偽 | 115〜170（turn_finished） | 1266〜1321 | 真 | 偽 | 偽 | 無し | — | — |
| 300 | 11-11T20:41:12 | 4772 | e4616dc1 | disk_space | — | 3.5 | landing_slot | 偽 | 178〜421（turn_finished） | 358〜601 | 真 | 不明 | 偽 | 無し | — | — |
| 301 | 11-11T20:58:05 | 4756 | a48e3733 | disk_space | — | 12.0 | landing_slot | 偽 | 169〜194（turn_finished） | 694〜719 | 真 | 真 | 偽 | 無し | — | — |
| 302 | 11-11T21:16:45 | 4704 | 04a7b432 | disk_space | — | 4.2 | landing_slot | 偽 | 52〜463（turn_finished） | 2368〜2779 | 真 | 真 | 偽 | 無し | — | — |
| 303 | 11-11T21:26:39 | 4709 | 644a44b4 | disk_space | — | 1.4 | landing_slot | 偽 | 91〜188（turn_finished） | 1775〜1871 | 真 | 真 | 偽 | 無し | — | — |
| 304 | 11-11T22:36:24 | 4791 | 371c5024 | awaiting_answer | 1688（approve_landing） | 10.2 | answer:approve_landing | 偽 | 18336〜18352（ask_opened） | 1397〜1413 | 真 | 真 | 真 | 1010 | 偽 | 真 |
| 305 | 11-11T22:38:57 | 4790 | fa619256 | awaiting_answer | 1686（approve_landing） | 8.8 | answer:approve_landing | 偽 | 21934〜21938（ask_opened） | 1198〜1201 | 真 | 真 | 真 | 1311 | 偽 | 偽 |
| 306 | 11-11T23:14:09 | 4312 | 202ac1e0 | awaiting_answer | 1697（approve_landing） | 10.4 | answer:approve_landing | 偽 | 0〜7（ask_opened） | 179〜185 | 偽 | — | 真 | 1441 | 偽 | 真 |
| 307 | 11-11T23:44:47 | 4666 | e0694e86 | disk_space | — | 10.1 | landing_slot | 真 | 149〜154（turn_finished） | 0〜5 | 偽 | — | 偽 | 無し | — | — |
| 308 | 11-11T23:47:09 | 4776 | 94d581cc | disk_space | — | 13.5 | landing_slot | 偽 | 146〜152（turn_finished） | 2431〜2437 | 偽 | — | 偽 | 無し | — | — |
| 309 | 11-12T01:16:17 | 4811 | d8f90d7c | disk_space | — | 12.4 | landing_slot | 偽 | 734〜748（turn_finished） | 8〜21 | 不明 | 不明 | 偽 | 無し | — | — |
| 310 | 11-12T01:44:00 | 4819 | 9c33e717 | disk_space | — | 1.4 | landing_slot | 偽 | 1631〜1884（turn_finished） | 171〜424 | 偽 | — | 偽 | 無し | — | — |
| 311 | 11-12T01:53:57 | 4790 | fa619256 | disk_space | — | 13.4 | landing_slot | 真 | 676〜683（turn_finished） | 0〜7 | 偽 | — | 偽 | 無し | — | — |
| 312 | 11-12T01:57:58 | 4791 | 371c5024 | disk_space | — | 10.2 | landing_slot | 偽 | 144〜150（turn_finished） | 2365〜2372 | 偽 | — | 偽 | 1638 | 偽 | 真 |
| 313 | 11-12T02:41:22 | 4663 | 3ed32af8 | disk_space | — | 12.2 | landing_slot | 偽 | 213〜220（turn_finished） | 346〜353 | 偽 | — | 偽 | 無し | — | — |
| 314 | 11-12T03:07:10 | 4791 | 371c5024 | disk_space | — | 10.2 | landing_slot | 偽 | 125〜135（turn_finished） | 898〜908 | 偽 | — | 偽 | 無し | — | — |
| 315 | 11-12T03:27:48 | 4823 | 7985a670 | disk_space | — | 6.6 | landing_slot | 偽 | 107〜1306（turn_finished） | 2660〜3858 | 偽 | — | 偽 | 699 | 偽 | 偽 |
| 316 | 11-12T03:54:00 | 4312 | 202ac1e0 | awaiting_answer | 1704（approve_landing） | 10.8 | answer:approve_landing | 偽 | 0〜18（ask_opened） | 3373〜3391 | 偽 | — | 真 | 1885 | 偽 | 偽 |
| 317 | 11-12T05:30:45 | 4132 | 5a406472 | disk_space | — | 14.3 | landing_slot | 偽 | 184〜191（turn_finished） | 6〜13 | 偽 | — | 偽 | 無し | — | — |
| 318 | 11-12T08:23:26 | 4793 | 924c2784 | disk_space | — | 22.5 | landing_slot | 偽 | 347〜369（turn_finished） | 1〜24 | 真 | 偽 | 偽 | 無し | — | — |
| 319 | 11-12T08:40:37 | 4806 | 33081081 | disk_space | — | 16.5 | landing_slot | 偽 | 563〜580（turn_finished） | 1807〜1825 | 真 | 偽 | 偽 | 1571 | 偽 | 真 |
| 320 | 11-12T09:12:35 | 4834 | babf73fc | disk_space | — | 14.0 | landing_slot | 偽 | 390〜396（turn_finished） | 2〜7 | 偽 | — | 偽 | 無し | — | — |
| 321 | 11-12T09:19:12 | 4834 | babf73fc | disk_space | — | 2.8 | landing_slot | 偽 | 732〜792（turn_finished） | 199〜259 | 真 | 真 | 偽 | 無し | — | — |
| 322 | 11-12T09:42:51 | 4312 | 202ac1e0 | disk_space | — | 15.9 | landing_slot | 偽 | 445〜452（turn_finished） | 3〜10 | 偽 | — | 偽 | 1598 | 偽 | 真 |
| 323 | 11-12T09:45:36 | 4806 | 33081081 | disk_space | — | 8.2 | landing_slot | 偽 | 421〜443（turn_finished） | 3225〜3247 | 真 | 真 | 偽 | 1034 | 偽 | 偽 |
| 324 | 11-12T09:50:05 | 4312 | 202ac1e0 | disk_space | — | 2.8 | landing_slot | 偽 | 773〜886（turn_finished） | 156〜269 | 不明 | 真 | 偽 | 1598 | 偽 | 真 |
| 325 | 11-12T09:56:58 | 4835 | 411b8608 | disk_space | — | 1.4 | landing_slot | 偽 | 60〜70（turn_finished） | 12950〜12960 | 不明 | 真 | 偽 | 無し | — | — |
| 326 | 11-12T10:08:34 | 4836 | fc4b2914 | disk_space | — | 1.4 | landing_slot | 偽 | 99〜218（turn_finished） | 12280〜12399 | 不明 | 不明 | 偽 | 無し | — | — |
| 327 | 11-12T13:22:12 | 4312 | 202ac1e0 | disk_space | — | 8.1 | needs_session_other | 偽 | 3443〜13613（turn_finished） | 4500〜14670 | 不明 | 不明 | 真 | 1598 | 偽 | 真 |
| 328 | 11-12T15:35:17 | 4312 | 202ac1e0 | disk_space | — | 7.9 | landing_slot | 偽 | 1802〜1854（turn_finished） | 1183〜1234 | 真 | 偽 | 偽 | 無し | — | — |
| 329 | 11-12T17:13:26 | 4122 | c8bccfff | awaiting_answer | 1717（approve_landing） | 20.7 | answer:approve_landing | 偽 | 0〜8（ask_opened） | 292〜300 | 真 | 偽 | 真 | 1194 | 偽 | 真 |
| 330 | 11-12T17:38:40 | 4122 | c8bccfff | awaiting_answer | 1717（approve_landing） | 6.6 | answer:approve_landing | 偽 | 1511〜1522（ask_opened） | 12593〜12604 | 真 | 偽 | 真 | 2214 | 偽 | 真 |
| 331 | 11-12T18:17:03 | 4629 | 0c5c2ca4 | awaiting_answer | 1718（approve_landing） | 17.5 | answer:approve_landing | 偽 | 0〜9（ask_opened） | 10856〜10866 | 真 | 真 | 真 | 1673 | 偽 | 真 |
| 332 | 11-12T18:36:02 | 4624 | 91799666 | awaiting_answer | 1719（approve_landing） | 7.7 | answer:approve_landing | 偽 | 0〜112（ask_opened） | 13594〜13705 | 不明 | 不明 | 真 | 1614 | 偽 | 偽 |
| 333 | 11-12T19:36:02 | 4625 | 80e441b3 | disk_space | — | 12.3 | landing_slot | 偽 | 180〜208（turn_finished） | 1〜29 | 真 | 偽 | 偽 | 無し | — | — |
| 334 | 11-12T19:36:11 | 4627 | 3e232900 | awaiting_answer | 1720（approve_landing） | 13.0 | answer:approve_landing | 偽 | 0〜8（ask_opened） | 10059〜10066 | 真 | 偽 | 真 | 1716 | 偽 | 真 |
| 335 | 11-12T19:39:22 | 4625 | 80e441b3 | disk_space | — | 2.9 | landing_slot | 偽 | 384〜408（turn_finished） | 811〜836 | 真 | 偽 | 偽 | 無し | — | — |
| 336 | 11-12T20:13:55 | 4842 | 8df0047d | disk_space | — | 16.1 | landing_slot | 偽 | 308〜325（turn_finished） | 73〜91 | 真 | 真 | 偽 | 無し | — | — |
| 337 | 11-12T21:45:48 | 4122 | c8bccfff | awaiting_answer | 1717（approve_landing） | 10.6 | answer:approve_landing | 偽 | 16343〜16350（ask_opened） | 2047〜2054 | 真 | 真 | 真 | 2271 | 偽 | 偽 |
| 338 | 11-12T21:47:25 | 4629 | 0c5c2ca4 | awaiting_answer | 1718（approve_landing） | 9.3 | answer:approve_landing | 偽 | 12553〜12632（ask_opened） | 2012〜2091 | 不明 | 不明 | 真 | 1776 | 偽 | 真 |
| 339 | 11-12T23:14:02 | 4122 | c8bccfff | disk_space | — | 10.6 | landing_slot | 偽 | 302〜334（turn_finished） | 1225〜1257 | 真 | 偽 | 偽 | 無し | — | — |
| 340 | 11-12T23:34:22 | 4843 | 972885aa | disk_space | — | 18.4 | landing_slot | 偽 | 158〜206（turn_finished） | 5〜53 | 真 | 偽 | 偽 | 無し | — | — |
| 341 | 11-12T23:40:09 | 4843 | 972885aa | disk_space | — | 2.9 | landing_slot | 偽 | 541〜553（turn_finished） | 2882〜2894 | 真 | 偽 | 偽 | 無し | — | — |
| 342 | 11-12T23:40:38 | 4629 | 0c5c2ca4 | awaiting_answer | 1724（approve_landing） | 13.0 | answer:approve_landing | 偽 | 0〜8（ask_opened） | 8412〜8420 | 真 | 偽 | 真 | 2107 | 偽 | 真 |
| 343 | 11-13T02:27:56 | 4624 | 91799666 | awaiting_answer | 1727（approve_landing） | 11.4 | answer:approve_landing | 偽 | 0〜16（ask_opened） | 63〜79 | 真 | 偽 | 真 | 1736 | 偽 | 偽 |
| 344 | 11-13T03:00:20 | 4627 | 3e232900 | awaiting_answer | 1728（approve_landing） | 7.8 | answer:approve_landing | 偽 | 0〜10（ask_opened） | 135〜145 | 真 | 偽 | 偽 | 2497 | 偽 | 真 |
| 345 | 11-13T03:43:39 | 4624 | 91799666 | disk_space | — | 9.5 | landing_slot | 偽 | 217〜245（turn_finished） | 531〜559 | 真 | 偽 | 偽 | 無し | — | — |
| 346 | 11-13T03:52:28 | 4627 | 3e232900 | disk_space | — | 8.4 | needs_session_other | 偽 | 3349〜3357（turn_finished） | 2997〜3005 | 真 | 偽 | 真 | 2497 | 偽 | 真 |
| 347 | 11-13T03:59:20 | 4624 | 91799666 | disk_space | — | 2.9 | landing_slot | 偽 | 1144〜1186（turn_finished） | 292〜334 | 真 | 偽 | 偽 | 無し | — | — |
| 348 | 11-13T04:28:18 | 4629 | 0c5c2ca4 | disk_space | — | 12.1 | landing_slot | 偽 | 13〜44（turn_finished） | 841〜872 | 真 | 偽 | 偽 | 無し | — | — |
| 349 | 11-13T05:27:58 | 4627 | 3e232900 | disk_space | — | 8.1 | landing_slot | 偽 | 192〜233（turn_finished） | 1467〜1508 | 真 | 偽 | 偽 | 無し | — | — |
| 350 | 11-13T05:42:21 | 4845 | ef119e19 | awaiting_answer | 1732（approve_landing） | 33.7 | answer:approve_landing | 偽 | 0〜11（ask_opened） | 70〜81 | 不明 | 不明 | 真 | 2785 | 偽 | 真 |
| 351 | 11-13T06:31:20 | 4845 | ef119e19 | disk_space | — | 13.3 | unknown:other:held|other:review_held | 偽 | — | — | 真 | 真 | 不明 | 1854 | 偽 | 真 |
| 352 | 11-13T06:50:31 | 4807 | 0149e149 | disk_space | 1735（worker_question） | 9.4 | answer:worker_question | 偽 | 4〜81（ask_opened） | 10751〜10828 | 不明 | 不明 | 真 | 5301 | 偽 | 真 |
| 353 | 11-13T10:18:46 | 4844 | 02efea29 | awaiting_answer | 1737（approve_landing） | 25.2 | answer:approve_landing | 偽 | 0〜9（ask_opened） | 565〜574 | 真 | 真 | 真 | 2702 | 偽 | 偽 |
| 354 | 11-13T11:13:19 | 4638 | b544b1f3 | disk_space | — | 11.0 | landing_slot | 偽 | 367〜417（turn_finished） | 2275〜2325 | 真 | 偽 | 偽 | 1250 | 偽 | 偽 |
| 355 | 11-13T11:18:46 | 4807 | 0149e149 | run_ended | — | 15.3 | waiting_no_ask | 真 | 13216〜16118（turn_finished） | 0〜2902 | 真 | 偽 | 真 | 2058 | 偽 | 真 |
| 356 | 11-13T11:51:13 | 4844 | 02efea29 | disk_space | — | 9.3 | landing_slot | 偽 | 354〜389（turn_finished） | 279〜314 | 真 | 偽 | 偽 | 1458 | 偽 | 偽 |
| 357 | 11-13T11:58:53 | 4808 | 442b26e5 | disk_space | — | 24.1 | landing_slot | 偽 | 579〜643（turn_finished） | 1426〜1490 | 真 | 偽 | 偽 | 無し | — | — |
| 358 | 11-13T12:02:06 | 4844 | 02efea29 | disk_space | — | 2.9 | landing_slot | 偽 | 1017〜1042（turn_finished） | 10620〜10645 | 真 | 偽 | 真 | 1458 | 偽 | 偽 |
| 359 | 11-13T12:22:54 | 4638 | b544b1f3 | disk_space | — | 8.4 | needs_session_other | 偽 | 4577〜4592（turn_finished） | 4151〜4165 | 真 | 偽 | 真 | 1250 | 偽 | 偽 |
| 360 | 11-13T13:02:54 | 4849 | c715915f | awaiting_answer | 1741（worker_question） | 13.7 | answer:worker_question | 偽 | 5〜28（ask_opened） | 189〜212 | 真 | 偽 | 真 | 2406 | 偽 | 偽 |
| 361 | 11-13T13:31:03 | 4807 | 0149e149 | disk_space | — | 7.8 | unknown:other:held|other:review_held | 偽 | — | — | 偽 | — | 不明 | 2547 | 偽 | 真 |
| 362 | 11-13T14:18:55 | 4807 | 0149e149 | awaiting_answer | 1744（approve_landing） | 11.1 | answer:approve_landing | 偽 | 0〜14（ask_opened） | 117〜131 | 真 | 偽 | 真 | 1278 | 偽 | 真 |
| 363 | 11-13T14:29:58 | 3162 | f3e1484c | awaiting_answer | 1745（approve_landing） | 14.4 | answer:approve_landing | 偽 | 0〜11（ask_opened） | 75〜85 | 真 | 偽 | 真 | 2408 | 偽 | 真 |
| 364 | 11-13T14:55:20 | 4807 | 0149e149 | disk_space | — | 7.3 | landing_slot | 偽 | 200〜245（turn_finished） | 1288〜1334 | 真 | 偽 | 偽 | 無し | — | — |
| 365 | 11-13T14:59:05 | 4845 | ef119e19 | awaiting_answer | 1746（approve_landing） | 7.8 | answer:approve_landing | 偽 | 0〜5（ask_opened） | 387〜393 | 真 | 偽 | 真 | 2505 | 偽 | 真 |
| 366 | 11-13T15:53:19 | 4844 | 02efea29 | awaiting_answer | 1747（approve_landing） | 12.2 | answer:approve_landing | 偽 | 0〜14（ask_opened） | 22080〜22094 | 真 | 偽 | 真 | 1420 | 偽 | 真 |
| 367 | 11-13T16:04:33 | 4845 | ef119e19 | awaiting_answer | 1748（approve_landing） | 12.6 | answer:approve_landing | 偽 | 0〜16（ask_opened） | 7541〜7557 | 真 | 偽 | 真 | 1793 | 偽 | 真 |
| 368 | 11-13T16:36:38 | 4849 | c715915f | disk_space | 1749（worker_question） | 8.1 | answer:worker_question | 偽 | 1〜41（ask_opened） | 21166〜21207 | 真 | 偽 | 真 | 84 | 偽 | 偽 |
| 369 | 11-13T16:52:42 | 3162 | f3e1484c | disk_space | — | 11.4 | landing_slot | 偽 | 427〜481（turn_finished） | 1810〜1863 | 真 | 偽 | 偽 | 無し | — | — |
| 370 | 11-13T18:40:30 | 4845 | ef119e19 | awaiting_answer | 1748（approve_landing） | 7.9 | answer:approve_landing | 偽 | 9367〜9373（ask_opened） | 13688〜13693 | 真 | 偽 | 真 | 2034 | 偽 | 真 |
| 371 | 11-13T21:00:30 | 4133 | ff0e1ccb | awaiting_answer | 1754（approve_landing） | 23.4 | answer:approve_landing | 偽 | 0〜30（ask_opened） | 6374〜6404 | 真 | 偽 | 真 | 1436 | 偽 | 真 |
| 372 | 11-13T22:25:26 | 4844 | 02efea29 | awaiting_answer | 1747（approve_landing） | 10.0 | answer:approve_landing | 偽 | 23535〜23541（ask_opened） | 134〜141 | 真 | 偽 | 偽 | 4 | 偽 | 偽 |
| 373 | 11-13T22:58:06 | 4890 | 36014d5b | run_ended | — | 12.8 | not_waiting | 偽 | — | — | 真 | 偽 | 不明 | 20 | 偽 | 偽 |
| 374 | 11-13T23:10:39 | 4845 | ef119e19 | awaiting_answer | 1755（approve_landing） | 11.2 | answer:approve_landing | 偽 | 0〜10（ask_opened） | 94〜104 | 真 | 偽 | 偽 | 無し | — | — |
| 375 | 11-13T23:16:23 | 4849 | c715915f | disk_space | — | 2.9 | landing_slot | 真 | 812〜820（turn_finished） | 0〜8 | 真 | 偽 | 偽 | 320 | 偽 | 偽 |
| 376 | 11-13T23:35:44 | 4133 | ff0e1ccb | disk_space | 1756（worker_question） | 8.8 | answer:worker_question | 偽 | 14〜106（ask_opened） | 160〜253 | 真 | 偽 | 真 | 3600 | 真 | 真 |
| 377 | 11-14T00:19:48 | 4134 | 618de801 | awaiting_answer | 1758（approve_landing） | 25.6 | answer:approve_landing | 偽 | 0〜17（ask_opened） | 1124〜1141 | 真 | 偽 | 真 | 2924 | 偽 | 偽 |
| 378 | 11-14T00:39:11 | 4133 | ff0e1ccb | disk_space | — | 10.1 | resume_after_limit | 偽 | 5〜16（turn_finished） | 2〜13 | 真 | 偽 | 真 | 2918 | 偽 | 偽 |
| 379 | 11-14T01:22:50 | 4849 | c715915f | disk_space | — | 8.5 | landing_answer_no_ask | 偽 | 8309〜8409（turn_finished） | 101〜200 | 真 | 偽 | 真 | 320 | 偽 | 偽 |
| 380 | 11-14T01:28:27 | 4844 | 02efea29 | disk_space | — | 2.9 | landing_slot | 真 | 10988〜11000（turn_finished） | 0〜12 | 真 | 偽 | 偽 | 4 | 偽 | 偽 |
| 381 | 11-14T01:59:04 | 4133 | ff0e1ccb | awaiting_answer | 1760（worker_question） | 11.1 | answer:worker_question | 偽 | 413〜431（ask_opened） | 263〜281 | 真 | 偽 | 真 | 1432 | 偽 | 真 |
| 382 | 11-14T02:15:16 | 4844 | 02efea29 | disk_space | — | 8.5 | needs_session_other | 偽 | 13740〜13810（turn_finished） | 2137〜2208 | 真 | 偽 | 真 | 4 | 偽 | 偽 |
| 383 | 11-14T02:23:04 | 4845 | ef119e19 | run_ended | — | 2.9 | needs_session_other | 真 | 11984〜11990（turn_finished） | 0〜6 | 真 | 偽 | 偽 | 無し | — | — |
| 384 | 11-14T02:51:13 | 4133 | ff0e1ccb | awaiting_answer | 1761（approve_landing） | 6.7 | answer:approve_landing | 偽 | 0〜40（ask_opened） | 37〜77 | 真 | 偽 | 偽 | 無し | — | — |
| 385 | 11-14T03:08:47 | 4849 | c715915f | awaiting_answer | 1762（approve_landing） | 11.7 | answer:approve_landing | 偽 | 0〜19（ask_opened） | 6532〜6551 | 真 | 偽 | 真 | 2 | 偽 | 偽 |
| 386 | 11-14T03:22:55 | 4844 | 02efea29 | disk_space | — | 7.2 | landing_slot | 偽 | 287〜359（turn_finished） | 917〜989 | 真 | 偽 | 偽 | 1902 | 偽 | 真 |
| 387 | 11-14T04:52:06 | 4942 | 9b6380c4 | disk_space | — | 6.7 | landing_slot | 偽 | 295〜381（turn_finished） | 5024〜5110 | 真 | 偽 | 偽 | 無し | — | — |
| 388 | 11-14T06:02:38 | 4844 | 02efea29 | disk_space | — | 11.7 | landing_answer_no_ask | 偽 | 9916〜9942（turn_finished） | 94969〜94994 | 真 | 偽 | 真 | 1902 | 偽 | 真 |
| 389 | 11-15T06:07:07 | 4845 | 48643d18 | awaiting_answer | 1771（approve_landing） | 11.5 | answer:approve_landing | 偽 | 0〜11（ask_opened） | 11936〜11947 | 真 | 偽 | 真 | 1155 | 偽 | 偽 |
| 390 | 11-15T06:43:55 | 4133 | f76f71ae | disk_space | — | 6.6 | landing_slot | 偽 | 231〜308（turn_finished） | 11〜88 | 真 | 偽 | 偽 | 無し | — | — |
| 391 | 11-15T07:07:47 | 4945 | 3d255155 | disk_space | — | 12.3 | resume_after_limit | 偽 | 7〜24（turn_finished） | 4〜20 | 真 | 偽 | 真 | 1366 | 偽 | 偽 |
| 392 | 11-15T07:58:39 | 4945 | 3d255155 | disk_space | — | 12.3 | landing_slot | 偽 | 257〜314（turn_finished） | 5805〜5863 | 真 | 偽 | 偽 | 736 | 偽 | 偽 |
| 393 | 11-15T08:10:39 | 4943 | e208f69b | disk_space | — | 12.4 | landing_slot | 偽 | 122〜151（turn_finished） | 1552〜1582 | 真 | 偽 | 偽 | 無し | — | — |
| 394 | 11-15T09:07:29 | 4844 | 02efea29 | disk_space | — | 8.1 | landing_slot | 真 | 574〜585（turn_finished） | 0〜11 | 真 | 偽 | 偽 | 無し | — | — |
| 395 | 11-15T09:13:54 | 4844 | 02efea29 | disk_space | — | 2.9 | landing_slot | 偽 | 895〜971（turn_finished） | 4177〜4253 | 真 | 真 | 偽 | 無し | — | — |
| 396 | 11-15T10:28:47 | 4890 | 36014d5b | disk_space | — | 9.0 | landing_slot | 偽 | 1970〜1997（turn_finished） | 5766〜5793 | 真 | 偽 | 偽 | 無し | — | — |
| 397 | 11-15T10:44:58 | 4945 | 3d255155 | disk_space | — | 8.4 | landing_slot | 偽 | 176〜244（turn_finished） | 11389〜11457 | 真 | 偽 | 真 | 1804 | 偽 | 真 |
| 398 | 11-15T12:05:50 | 4844 | 02efea29 | disk_space | — | 8.5 | landing_answer_no_ask | 偽 | 11231〜11287（turn_finished） | 6519〜6576 | 真 | 真 | 偽 | 無し | — | — |
| 399 | 11-15T13:27:48 | 4979 | f7bf5265 | disk_space | — | 6.5 | landing_slot | 偽 | 217〜233（turn_finished） | 1531〜1547 | 真 | 偽 | 偽 | 無し | — | — |
| 400 | 11-15T14:02:35 | 4845 | 48643d18 | disk_space | — | 10.9 | landing_slot | 真 | 281〜300（turn_finished） | 0〜19 | 偽 | — | 偽 | 1654 | 偽 | 偽 |
| 401 | 11-15T14:09:00 | 4845 | 48643d18 | disk_space | — | 2.9 | landing_slot | 偽 | 668〜686（turn_finished） | 569〜587 | 偽 | — | 偽 | 1654 | 偽 | 偽 |
| 402 | 11-15T14:27:47 | 4945 | 3d255155 | disk_space | — | 8.3 | landing_slot | 偽 | 154〜164（turn_finished） | 1788〜1799 | 偽 | — | 偽 | 無し | — | — |
| 403 | 11-15T15:08:33 | 4845 | 48643d18 | disk_space | — | 8.5 | needs_session_other | 偽 | 3601〜4259（turn_finished） | 10382〜11040 | 不明 | 不明 | 真 | 1654 | 偽 | 偽 |
| 404 | 11-15T16:02:28 | 4950 | 708f6564 | disk_space | — | 12.2 | landing_slot | 偽 | 148〜155（turn_finished） | 3〜10 | 真 | 真 | 偽 | 無し | — | — |
| 405 | 11-15T16:09:06 | 4950 | 708f6564 | disk_space | — | 2.9 | landing_slot | 真 | 534〜552（turn_finished） | 0〜17 | 偽 | — | 偽 | 無し | — | — |
| 406 | 11-15T17:03:49 | 4984 | 03dba482 | disk_space | — | 13.9 | landing_slot | 偽 | 269〜278（turn_finished） | 714〜723 | 偽 | — | 偽 | 無し | — | — |
| 407 | 11-15T17:22:15 | 4991 | e6b8d284 | disk_space | — | 5.4 | landing_slot | 偽 | 158〜520（turn_finished） | 3009〜3371 | 真 | 偽 | 偽 | 無し | — | — |
| 408 | 11-15T17:52:45 | 4944 | 23dc78cb | disk_space | — | 5.6 | landing_slot | 偽 | 112〜158（turn_finished） | 523〜569 | 真 | 偽 | 偽 | 無し | — | — |
| 409 | 11-15T20:08:25 | 4981 | fb5dc8e1 | disk_space | — | 12.7 | landing_slot | 偽 | 129〜140（turn_finished） | 945〜955 | 真 | 偽 | 偽 | 1166 | 偽 | 偽 |
| 410 | 11-15T21:45:23 | 4849 | 4e12b44c | disk_space | 1783（worker_question） | 8.6 | answer:worker_question | 偽 | 4〜51（ask_opened） | 1209〜1256 | 真 | 真 | 真 | 101 | 偽 | 偽 |
| 411 | 11-15T22:46:50 | 4844 | 1ccbd6eb | awaiting_answer | 1786（approve_landing） | 14.3 | answer:approve_landing | 偽 | 0〜93（ask_opened） | 0〜93 | 不明 | 不明 | 偽 | 1166 | 偽 | 真 |
| 412 | 11-15T23:04:38 | 4997 | dbfea376 | disk_space | — | 8.5 | landing_slot | 偽 | 172〜234（turn_finished） | 1288〜1350 | 真 | 偽 | 偽 | 無し | — | — |
| 413 | 11-15T23:42:29 | 4844 | 1ccbd6eb | disk_space | — | 2.9 | landing_slot | 偽 | 3672〜3726（turn_finished） | 487〜542 | 真 | 偽 | 偽 | 1166 | 偽 | 真 |
| 414 | 11-16T00:15:55 | 4844 | 1ccbd6eb | awaiting_answer | 1789（approve_landing） | 7.3 | answer:approve_landing | 偽 | 0〜43（ask_opened） | 197〜241 | 真 | 偽 | 偽 | 無し | — | — |
| 415 | 11-16T00:49:09 | 4985 | fce0fbf6 | run_ended | — | 7.1 | not_waiting | 偽 | — | — | 真 | 偽 | 不明 | 4441 | 偽 | 偽 |
| 416 | 11-16T00:49:09 | 5009 | 2d9bfb96 | run_ended | — | 5.4 | not_waiting | 偽 | — | — | 真 | 偽 | 不明 | 7267 | 偽 | 真 |
| 417 | 11-16T02:45:06 | 4985 | fce0fbf6 | disk_space | 1792（worker_question） | 3.8 | answer:worker_question | 偽 | 55〜80（ask_opened） | 0〜25 | 真 | 真 | 真 | 29 | 偽 | 偽 |
| 418 | 11-16T02:51:07 | 5009 | 2d9bfb96 | run_ended | — | 5.5 | not_waiting | 偽 | — | — | 真 | 真 | 不明 | 無し | — | — |
| 419 | 11-16T04:54:49 | 5009 | 63c7a785 | awaiting_answer | 1795（approve_landing） | 11.7 | answer:approve_landing | 偽 | 0〜13（ask_opened） | 1891〜1905 | 真 | 真 | 偽 | 無し | — | — |
| 420 | 11-16T06:43:20 | 4956 | 9d26757e | disk_space | — | 10.1 | landing_slot | 偽 | 152〜185（turn_finished） | 143〜176 | 真 | 真 | 偽 | 無し | — | — |
| 421 | 11-16T08:28:22 | 5059 | 148d3ecd | disk_space | — | 11.4 | landing_slot | 偽 | 221〜240（turn_finished） | 93〜112 | 真 | 偽 | 偽 | 無し | — | — |
| 422 | 11-16T08:30:58 | 4891 | 14892a48 | disk_space | — | 11.0 | landing_slot | 偽 | 564〜626（turn_finished） | 2747〜2810 | 真 | 偽 | 偽 | 無し | — | — |
| 423 | 11-16T08:39:15 | 5059 | 148d3ecd | disk_space | — | 2.9 | landing_slot | 偽 | 834〜893（turn_finished） | 6178〜6236 | 真 | 偽 | 偽 | 無し | — | — |
| 424 | 11-16T08:52:03 | 5011 | ba356862 | run_ended | — | 15.2 | not_waiting | 偽 | — | — | 偽 | — | 不明 | 3288 | 偽 | 真 |
| 425 | 11-16T09:11:29 | 4946 | abd40aeb | disk_space | — | 5.5 | landing_slot | 偽 | 262〜283（turn_finished） | 4854〜4875 | 不明 | 真 | 偽 | 無し | — | — |
| 426 | 11-16T09:48:06 | 4893 | 7936c268 | disk_space | — | 4.3 | landing_slot | 偽 | 1759〜1780（turn_finished） | 2657〜2678 | 真 | 真 | 偽 | 無し | — | — |
| 427 | 11-16T10:34:51 | 4891 | 14892a48 | disk_space | — | 8.8 | unknown:landing_answer_no_ask|other:held | 偽 | — | — | 偽 | — | 不明 | 不明 | 不明 | 不明 |
| 428 | 11-16T10:49:44 | 4946 | abd40aeb | disk_space | — | 5.5 | landing_answer_no_ask | 偽 | 6147〜6177（turn_finished） | 515〜546 | 偽 | — | 偽 | 無し | — | — |
| 429 | 11-16T10:51:21 | 4906 | eb4e8e88 | disk_space | — | 1.5 | landing_slot | 偽 | 5349〜5454（turn_finished） | 329〜434 | 偽 | — | 偽 | 無し | — | — |
| 430 | 11-16T10:51:21 | 4897 | ec58ba7a | disk_space | — | 3.0 | landing_slot | 偽 | 5330〜5435（turn_finished） | 35〜140 | 偽 | — | 偽 | 無し | — | — |
| 431 | 11-16T11:03:34 | 4921 | 304f55d6 | disk_space | — | 3.0 | landing_slot | 偽 | 110〜130（turn_finished） | 298〜318 | 偽 | — | 偽 | 無し | — | — |
| 432 | 11-16T12:10:55 | 5011 | ba356862 | disk_space | 1809（worker_question） | 10.7 | answer:worker_question | 偽 | 4〜56（ask_opened） | 147〜199 | 真 | 真 | 真 | 3601 | 真 | 真 |
| 433 | 11-16T12:15:00 | 5027 | 57151383 | disk_space | 1810（worker_question） | 0.5 | answer:worker_question | 偽 | 5〜243（ask_opened） | 254〜491 | 偽 | — | 真 | 9829 | 偽 | 偽 |
| 434 | 11-16T12:39:28 | 5064 | 57ae6e42 | awaiting_answer | 1811（approve_landing） | 12.3 | answer:approve_landing | 偽 | 0〜19（ask_opened） | 734〜753 | 真 | 真 | 真 | 3272 | 偽 | 真 |
| 435 | 11-16T12:48:20 | 5060 | 95b3ceaa | disk_space | — | 12.3 | landing_slot | 真 | 426〜435（turn_finished） | 0〜8 | 偽 | — | 偽 | 無し | — | — |
| 436 | 11-16T13:14:02 | 5011 | ba356862 | disk_space | — | 10.6 | resume_after_limit | 偽 | 5〜21（turn_finished） | 750〜767 | 偽 | — | 真 | 3672 | 偽 | 偽 |
| 437 | 11-16T13:37:39 | 5108 | 58f0bf06 | awaiting_answer | 1814（approve_landing） | 13.0 | answer:approve_landing | 偽 | 0〜9（ask_opened） | 173〜182 | 真 | 偽 | 真 | 4180 | 偽 | 偽 |
| 438 | 11-16T14:23:57 | 5064 | 57ae6e42 | run_ended | — | 4.2 | not_waiting | 偽 | — | — | 真 | 偽 | 不明 | ≥ 13088（未終了） | 不明 | 不明 |
| 439 | 11-16T14:51:28 | 5064 | 57ae6e42 | disk_space | — | 1.5 | unknown:needs_session_other|other:held | 偽 | — | — | 真 | 偽 | 不明 | 1663 | 偽 | 偽 |
| 440 | 11-16T15:17:52 | 5108 | 58f0bf06 | disk_space | — | 6.8 | landing_slot | 偽 | 304〜324（turn_finished） | 1665〜1684 | 真 | 偽 | 偽 | 1077 | 偽 | 真 |
| 441 | 11-16T16:01:08 | 5108 | 58f0bf06 | disk_space | — | 8.6 | needs_session_other | 偽 | 2873〜2920（turn_finished） | 1282〜1329 | 真 | 偽 | 真 | 1077 | 偽 | 真 |
| 442 | 11-16T16:45:52 | 5108 | 58f0bf06 | disk_space | — | 6.9 | landing_slot | 偽 | 274〜324（turn_finished） | 2〜52 | 真 | 偽 | 偽 | 無し | — | — |
| 443 | 11-16T17:37:23 | 5064 | 57ae6e42 | disk_space | — | 13.9 | landing_slot | 偽 | 416〜455（turn_finished） | 1443〜1482 | 真 | 偽 | 偽 | 無し | — | — |
</details>

### 未終了・不明・左端

- 待ちが t₀ で終わっていない行は 2 件。task 4175 の 2026-11-11T03:19:21 は、`needs_session` の区間は `held` に移って終わったが、答えも resume も着地も t₀ までに無いので、待ちは未終了（消してから待ちの終わりは ≥ 484,996 秒の下限）。task 4891 の 2026-11-16T10:34:51 は、`landing_answer` から `held` に移り t₀ で未終了で、候補の `held` の区間が t₀ で開いたままで、どちらの区間で消したかも決まらない（種類は unknown）。この 2 件は戻ったか・次の turn・上限・また消されたかを不明とした。
- 種類の決まらない行は 10 件、待ちの外の行（not_waiting）は 18 件で、どちらも e は不明（合わせて 28 件）。種類の決まった行で起点が記録に無い行は 0 件。
- 削除の時の保留が決まらない行は 51 件、保留のありうる行で 10 分で解けたかが決まらない行は 40 件。
- 消した後の最初の turn がある行は 242 件、無いと決まった行は 199 件、不明は 2 件。上限で止まったのは 3 件、決まらないのは 1 件（t₀ で未終了の 13,088 秒の turn。上限 7200 秒を超えて終わりの event が無いので、記録の欠けか t₀ に動いている turn）。また消されたのは 147 件、決まらないのは 1 件。
- 左端より前に起点があり区間の中で終わった待ちの区間は 8 件、turn は 1 件で、Q2 と turn の比べから外した。

### 消した後の最初の turn と消していない turn

始まりが観測区間の中の turn。種類は turn の始まりの attempt の kind。

| 組 | 件数 | 決まった件数 | 未終了（経過 秒） | 中央値 秒 | 90% 秒 | 上限で止まった | 割合（決まった行） |
|---|---|---|---|---|---|---|---|
| resume・消した後 | 216 | 215 | 1（13088） | 1432 | 2816 | 3 | 1.4% |
| resume・消していない | 73 | 73 | 0（—） | 758 | 3447 | 4 | 5.5% |
| revise・消した後 | 5 | 5 | 0（—） | 84 | 347 | 0 | 0.0% |
| revise・消していない | 379 | 378 | 1（438258） | 502 | 1661 | 2 | 0.5% |
| revise・resume・消した後 | 221 | 220 | 1（13088） | 1398 | 2790 | 3 | 1.4% |
| revise・resume・消していない | 452 | 451 | 1（438258） | 547 | 1907 | 6 | 1.3% |

2 通りの置き方（未終了の turn の長さを、消した後の組で下限・消していない組で ∞ とする置き方とその逆、上限の割合は description の 2 通り）：

| 組 | 中央値の差（下限・∞） | 中央値の差（∞・下限） | 上限の割合の差の最小 | 最大 |
|---|---|---|---|---|
| resume どうし | +674 秒 | +674 秒 | −4.09 ポイント | −3.63 ポイント |
| revise・resume | +854 秒 | +854 秒 | −0.19 ポイント | +0.48 ポイント |

- 消した後の turn が長いのは、作り直しの build だけでなく turn の中身の違いも入る。消した後の resume の多くは approve_landing の答え（send_back の「指摘をすべて直す」）の後で、消していない resume は検証の失敗の直しや rebase が多い。差 674〜854 秒は Q1 の Δt(A) 308.5 秒・Δt(I) 339.5 秒より大きく、build の延びだけでは説明しきれない。
- 悪循環（消した後の最初の turn が上限で止まり、その後にまた消された）は 3 件：task 4175（2026-11-10T13:50:41）、4133（2026-11-13T23:35:44）、5011（2026-11-16T12:10:55）。次の turn・上限・また消されたかのどれかが決まらず悪循環かが決まらない行は 3 件。

## Q1：手直しの最初の build の延び

### 毎回の行

順は K→I→I→K→A→I→K→K→I→A（K と I は ABBA、A は K・I の 4 回目と 8 回目の後）。秒は 1 本ごとの壁時計の秒（括弧は cargo の Finished の値）、T は 3 本の和。GiB は `du -sk` を 1024² で割った値。写し取りの確認は 10 回とも 1 回目で一致した（hard link の組 1050 組が S と同じ、target 13,024,188 KiB・incremental 11,127,880 KiB で S と同じ）。S を作ったときの確認は、元の target（13,159,156 KiB・incremental 11,248,152 KiB。setup.sh の直後の 13,191,920 KiB との差は build の後の小さな書き込み）と S（13,024,188 KiB・11,127,880 KiB）で、組 1050 組が一致し、du の差は 1.0%・1.1% で 2% の内だった。sccache の hit/miss の差は server 全体の数（同時の他の run の compile も入る）。

| 回 | 対 | 条件 | lib の秒 | it の秒 | clippy の秒 | T | 消す前の target | 消した GiB | build の後の target | sccache hit/miss | load 始め | load 終わり | Compiling・Checking の行 | 写し取りの確認 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 1 | K | 62（1m 01s） | 8（8.16s） | 46（45.58s） | 116 | 12.42 | 0 | 12.35 | 0/0 | 7.83 | 4.22 | 3 | 1050 組・一致 |
| 2 | 1 | I | 255（4m 15s） | 40（39.51s） | 195（3m 14s） | 490 | 12.42 | 9.04 | 9.37 | 0/0 | 4.04 | 8.30 | 3 | 1050 組・一致 |
| 3 | 2 | I | 258（4m 18s） | 56（56.55s） | 144（2m 22s） | 458 | 12.42 | 9.04 | 10.45 | 0/0 | 7.72 | 3.88 | 3 | 1050 組・一致 |
| 4 | 2 | K | 88（1m 27s） | 11（10.81s） | 63（1m 02s） | 162 | 12.42 | 0 | 12.37 | 0/0 | 3.89 | 8.97 | 3 | 1050 組・一致 |
| 5 | — | A | 251（4m 11s） | 32（31.71s） | 129（2m 09s） | 412 | 12.42 | 12.42 | 7.97 | 202/0 | 8.50 | 3.45 | 204 | 1050 組・一致 |
| 6 | 3 | I | 233（3m 52s） | 33（32.86s） | 131（2m 10s） | 397 | 12.42 | 9.04 | 9.35 | 0/0 | 3.33 | 3.40 | 3 | 1050 組・一致 |
| 7 | 3 | K | 59（58.97s） | 7（7.54s） | 44（43.37s） | 110 | 12.42 | 0 | 12.35 | 0/0 | 4.25 | 3.32 | 3 | 1050 組・一致 |
| 8 | 4 | K | 85（1m 25s） | 12（11.65s） | 56（56.03s） | 153 | 12.42 | 0 | 12.36 | 0/0 | 3.37 | 6.47 | 3 | 1050 組・一致 |
| 9 | 4 | I | 350（5m 49s） | 43（43.02s） | 162（2m 42s） | 555 | 12.42 | 9.04 | 9.38 | 0/0 | 6.47 | 4.81 | 3 | 1050 組・一致 |
| 10 | — | A | 278（4m 37s） | 37（37.42s） | 159（2m 38s） | 474 | 12.42 | 12.42 | 7.97 | 202/0 | 4.75 | 5.07 | 204 | 1050 組・一致 |

- 3 本とも全ての回で終了の status 0。K と I の build は `dagq` の crate（lib・it・clippy で 1 行ずつ）だけを compile し、A は依存の crate を含む 204 行で、依存は sccache の hit（202）だった。
- I の build の後の target が K より 2〜3 GiB 小さいのは、消した incremental の 1 つ前の session が戻らないため（rustc は build の後に今の session と 1 つ前の session を残す）。A の後の 7.97 GiB も同じ理由で、次の build の後には K の大きさに近づくと見込む（測っていない）。
- 15 分を超えた build、30 分を超えた回、load が 16 を超えた回は無い。

### 集計

| 条件 | 有効な回 | T | T の中央値 | Δt(X) | 消した GiB の中央値 G(X) | 判定 |
|---|---|---|---|---|---|---|
| K | 4 | 116・162・110・153 | 134.5 | — | 0 | — |
| I | 4 | 490・458・397・555 | 474 | 339.5 | 9.04 | unresolved |
| A | 2 | 412・474 | 443 | 308.5 | 12.42 | does_not_hold |

G_run（有効な K の build の後の target の中央値）= 12.355 GiB。

I の ABBA の対：

| 対 | 回 | K の T | I の T | 延び（I − K） | 比（I ÷ K） |
|---|---|---|---|---|---|
| 1 | 1・2 | 116 | 490 | 374 | 4.22 |
| 2 | 3・4 | 162 | 458 | 296 | 2.83 |
| 3 | 6・7 | 110 | 397 | 287 | 3.61 |
| 4 | 8・9 | 153 | 555 | 402 | 3.63 |


## Q2：待ちの種類と経過ごとの消す条件

観測区間 t₀ − 14 日〜t₀、対象の run は 374 本。待ちの区間は phase の変わり目と ask の作成・答え・close の時刻で切り、起点（ask の作成か直前の turn_finished）が観測区間の中にある 1550 件（未終了 2 件、戻ったかが不明 5 件）を使った。
左端より前に起点があり区間の中で終わった 8 件は外した。

起点が記録に無い区間（ask が無く、run の始まりまで遡っても区間の前に turn_finished が無い）は、始まりが観測区間の中のもので 10 件（waiting_no_ask 9 件、landing_slot 1 件）で、e と L が決まらないので表から外した。
判定への影響：この 10 件だけ起点を、区間の始まり（L が最も短い）と観測区間の左端（L が最も長い）の両端に置いた入力（`WAITS=waits_lo.json`・`waits_hi.json`。ほかの区間は waits2.json のまま）で同じ表を作り直すと、waiting_no_ask は e = 0 の区間が 32 件、landing_slot は 786 件になり、どちらの置き方でも e_r(A) は 0 分のままで、判定は変わらない。

表は `python3 -I q2b.py 9.04 12.42 12.355 339.5 308.5`（G(I)・G(A)・G_run・Δt(I)・Δt(A)）で出した。M_r(e, X) = (G(X) ÷ G_run) × R_r(e) ÷ (Δt(X) × p_r(e))。I は Q1 が unresolved なので e_r(I) は出さず、表の I の列は参考に残す。A は G(A) = 12.42、Δt(A) = 308.5 で出した。R は秒。n_r(e) ≧ 10 の e で「不明」になった行は無かった。


#### landing_slot（e = 0 の区間 785 件、e_r(A) = 0 分）

| e 分 | n | 未終了（下限 > e） | 未終了（下限 ≦ e、除く） | 戻ったか不明 | R⁻ 秒 | R⁺ 秒 | R の 90% 秒 | p⁺ | p⁻ | M⁻(I) | M⁺(I) | I | M⁻(A) | M⁺(A) | A |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 785 | 0 | 0 | 0 | 1068 | 1067.75 | 5417 | 0.03 | 0.03 | 90.32 | 90.32 | 見合う | 136.56 | 136.56 | 見合う |
| 5 | 644 | 0 | 0 | 0 | 1268 | 1268.29 | 5499 | 0.03 | 0.03 | 92.65 | 92.65 | 見合う | 140.08 | 140.08 | 見合う |
| 10 | 489 | 0 | 0 | 0 | 1484 | 1483.83 | 6064 | 0.04 | 0.04 | 82.30 | 82.30 | 見合う | 124.44 | 124.44 | 見合う |
| 15 | 413 | 0 | 0 | 0 | 1523 | 1522.58 | 6440 | 0.04 | 0.04 | 75.29 | 75.29 | 見合う | 113.84 | 113.84 | 見合う |
| 30 | 294 | 0 | 0 | 0 | 1529 | 1529.38 | 7982 | 0.05 | 0.05 | 64.60 | 64.60 | 見合う | 97.68 | 97.68 | 見合う |
| 60 | 135 | 0 | 0 | 0 | 2133 | 2133.12 | 11016 | 0.07 | 0.07 | 62.06 | 62.06 | 見合う | 93.84 | 93.84 | 見合う |
| 120 | 42 | 0 | 0 | 0 | 5613 | 5613.06 | 21532 | 0.10 | 0.10 | 127.02 | 127.02 | 見合う | 192.05 | 192.05 | 見合う |

#### needs_session_other（e = 0 の区間 204 件、e_r(A) = 0 分）

| e 分 | n | 未終了（下限 > e） | 未終了（下限 ≦ e、除く） | 戻ったか不明 | R⁻ 秒 | R⁺ 秒 | R の 90% 秒 | p⁺ | p⁻ | M⁻(I) | M⁺(I) | I | M⁻(A) | M⁺(A) | A |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 204 | 1 | 0 | 2 | 4845 | 4844.64 | 27071 | 0.94 | 0.93 | 11.09 | 11.21 | 見合う | 16.77 | 16.95 | 見合う |
| 5 | 192 | 1 | 0 | 2 | 5525 | 5525.14 | 27112 | 0.94 | 0.93 | 12.70 | 12.84 | 見合う | 19.20 | 19.42 | 見合う |
| 10 | 177 | 1 | 0 | 2 | 5778 | 5777.65 | 27068 | 0.94 | 0.93 | 13.28 | 13.44 | 見合う | 20.07 | 20.32 | 見合う |
| 15 | 170 | 1 | 0 | 2 | 6003 | 6002.58 | 26832 | 0.94 | 0.92 | 13.83 | 14.01 | 見合う | 20.91 | 21.18 | 見合う |
| 30 | 144 | 1 | 0 | 2 | 6938 | 6938.16 | 26571 | 0.93 | 0.92 | 16.07 | 16.31 | 見合う | 24.30 | 24.66 | 見合う |
| 60 | 117 | 1 | 0 | 2 | 8063 | 8062.54 | 27458 | 0.93 | 0.91 | 18.65 | 19.00 | 見合う | 28.20 | 28.73 | 見合う |
| 120 | 83 | 1 | 0 | 2 | 9741 | 9740.84 | 41016 | 0.95 | 0.93 | 22.06 | 22.63 | 見合う | 33.35 | 34.21 | 見合う |

#### answer:approve_landing（e = 0 の区間 183 件、e_r(A) = 0 分）

| e 分 | n | 未終了（下限 > e） | 未終了（下限 ≦ e、除く） | 戻ったか不明 | R⁻ 秒 | R⁺ 秒 | R の 90% 秒 | p⁺ | p⁻ | M⁻(I) | M⁺(I) | I | M⁻(A) | M⁺(A) | A |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 183 | 0 | 0 | 0 | 4603 | 4602.96 | 40320 | 0.79 | 0.79 | 12.61 | 12.61 | 見合う | 19.06 | 19.06 | 見合う |
| 5 | 146 | 0 | 0 | 0 | 9028 | 9027.90 | 50362 | 0.79 | 0.79 | 24.70 | 24.70 | 見合う | 37.35 | 37.35 | 見合う |
| 10 | 138 | 0 | 0 | 0 | 9721 | 9721.48 | 52750 | 0.78 | 0.78 | 26.77 | 26.77 | 見合う | 40.48 | 40.48 | 見合う |
| 15 | 132 | 0 | 0 | 0 | 10184 | 10183.61 | 54613 | 0.78 | 0.78 | 28.13 | 28.13 | 見合う | 42.53 | 42.53 | 見合う |
| 30 | 116 | 0 | 0 | 0 | 11173 | 11172.61 | 54249 | 0.80 | 0.80 | 30.03 | 30.03 | 見合う | 45.41 | 45.41 | 見合う |
| 60 | 99 | 0 | 0 | 0 | 11044 | 11043.97 | 53916 | 0.82 | 0.82 | 29.09 | 29.09 | 見合う | 43.98 | 43.98 | 見合う |
| 120 | 79 | 0 | 0 | 0 | 12363 | 12363.07 | 56859 | 0.86 | 0.86 | 30.95 | 30.95 | 見合う | 46.80 | 46.80 | 見合う |

#### landing_answer_no_ask（e = 0 の区間 175 件、e_r(A) = 0 分）

| e 分 | n | 未終了（下限 > e） | 未終了（下限 ≦ e、除く） | 戻ったか不明 | R⁻ 秒 | R⁺ 秒 | R の 90% 秒 | p⁺ | p⁻ | M⁻(I) | M⁺(I) | I | M⁻(A) | M⁺(A) | A |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 175 | 0 | 0 | 1 | 3825 | 3825.01 | 20487 | 0.70 | 0.69 | 11.82 | 11.92 | 見合う | 17.88 | 18.03 | 見合う |
| 5 | 157 | 0 | 0 | 1 | 5018 | 5018.31 | 20984 | 0.71 | 0.71 | 15.16 | 15.30 | 見合う | 22.92 | 23.13 | 見合う |
| 10 | 137 | 0 | 0 | 1 | 6093 | 6092.68 | 22939 | 0.70 | 0.69 | 18.74 | 18.94 | 見合う | 28.33 | 28.63 | 見合う |
| 15 | 132 | 0 | 0 | 1 | 5893 | 5892.50 | 23678 | 0.70 | 0.70 | 18.03 | 18.22 | 見合う | 27.25 | 27.55 | 見合う |
| 30 | 111 | 0 | 0 | 1 | 7791 | 7791.28 | 23506 | 0.69 | 0.68 | 24.21 | 24.52 | 見合う | 36.60 | 37.08 | 見合う |
| 60 | 89 | 0 | 0 | 1 | 8035 | 8035.11 | 24909 | 0.70 | 0.69 | 24.86 | 25.27 | 見合う | 37.58 | 38.20 | 見合う |
| 120 | 64 | 0 | 0 | 1 | 6690 | 6689.82 | 25724 | 0.77 | 0.75 | 18.83 | 19.22 | 見合う | 28.47 | 29.07 | 見合う |

#### other:awaiting_e2e（e = 0 の区間 90 件、表の機械的な e_r(A) = 0 分。3 本の run の断片なので標本不足で unresolved）

| e 分 | n | 未終了（下限 > e） | 未終了（下限 ≦ e、除く） | 戻ったか不明 | R⁻ 秒 | R⁺ 秒 | R の 90% 秒 | p⁺ | p⁻ | M⁻(I) | M⁺(I) | I | M⁻(A) | M⁺(A) | A |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 90 | 0 | 0 | 0 | 475 | 475.43 | 632 | 0.00 | 0.00 | ∞ | ∞ | 見合う | ∞ | ∞ | 見合う |
| 5 | 68 | 0 | 0 | 0 | 230 | 230.03 | 340 | 0.00 | 0.00 | ∞ | ∞ | 見合う | ∞ | ∞ | 見合う |
| 10 | 17 | 0 | 0 | 0 | 35 | 35.07 | 60 | 0.00 | 0.00 | ∞ | ∞ | 見合う | ∞ | ∞ | 見合う |
| 15 | 0 | 0 | 0 | — | — | — | — | — | — | — | — | — | — | — | — |
| 30 | 0 | 0 | 0 | — | — | — | — | — | — | — | — | — | — | — | — |
| 60 | 0 | 0 | 0 | — | — | — | — | — | — | — | — | — | — | — | — |
| 120 | 0 | 0 | 0 | — | — | — | — | — | — | — | — | — | — | — | — |

#### answer:worker_question（e = 0 の区間 25 件、e_r(A) = 0 分）

| e 分 | n | 未終了（下限 > e） | 未終了（下限 ≦ e、除く） | 戻ったか不明 | R⁻ 秒 | R⁺ 秒 | R の 90% 秒 | p⁺ | p⁻ | M⁻(I) | M⁺(I) | I | M⁻(A) | M⁺(A) | A |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 25 | 0 | 0 | 0 | 1901 | 1900.64 | 26078 | 1.00 | 1.00 | 4.10 | 4.10 | 見合う | 6.19 | 6.19 | 見合う |
| 5 | 19 | 0 | 0 | 0 | 15469 | 15469.11 | 35453 | 1.00 | 1.00 | 33.34 | 33.34 | 見合う | 50.41 | 50.41 | 見合う |
| 10 | 17 | 0 | 0 | 0 | 16827 | 16827.28 | 44799 | 1.00 | 1.00 | 36.27 | 36.27 | 見合う | 54.83 | 54.83 | 見合う |
| 15 | 15 | 0 | 0 | 0 | 18585 | 18584.76 | 54145 | 1.00 | 1.00 | 40.05 | 40.05 | 見合う | 60.56 | 60.56 | 見合う |
| 30 | 14 | 0 | 0 | 0 | 18546 | 18546.15 | 58068 | 1.00 | 1.00 | 39.97 | 39.97 | 見合う | 60.43 | 60.43 | 見合う |
| 60 | 11 | 0 | 0 | 0 | 19503 | 19503.24 | 70737 | 1.00 | 1.00 | 42.03 | 42.03 | 見合う | 63.55 | 63.55 | 見合う |
| 120 | 11 | 0 | 0 | 0 | 15903 | 15903.24 | 67137 | 1.00 | 1.00 | 34.27 | 34.27 | 見合う | 51.82 | 51.82 | 見合う |

#### waiting_no_ask（e = 0 の区間 23 件、e_r(A) = 0 分）

| e 分 | n | 未終了（下限 > e） | 未終了（下限 ≦ e、除く） | 戻ったか不明 | R⁻ 秒 | R⁺ 秒 | R の 90% 秒 | p⁺ | p⁻ | M⁻(I) | M⁺(I) | I | M⁻(A) | M⁺(A) | A |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 23 | 0 | 0 | 0 | 10819 | 10819.37 | 26129 | 1.00 | 1.00 | 23.32 | 23.32 | 見合う | 35.26 | 35.26 | 見合う |
| 5 | 18 | 0 | 0 | 0 | 15639 | 15638.68 | 40319 | 1.00 | 1.00 | 33.70 | 33.70 | 見合う | 50.96 | 50.96 | 見合う |
| 10 | 17 | 0 | 0 | 0 | 15518 | 15517.99 | 44842 | 1.00 | 1.00 | 33.44 | 33.44 | 見合う | 50.57 | 50.57 | 見合う |
| 15 | 16 | 0 | 0 | 0 | 16933 | 16932.80 | 49364 | 1.00 | 1.00 | 36.49 | 36.49 | 見合う | 55.18 | 55.18 | 見合う |
| 30 | 15 | 0 | 0 | 0 | 17748 | 17747.62 | 53286 | 1.00 | 1.00 | 38.25 | 38.25 | 見合う | 57.83 | 57.83 | 見合う |
| 60 | 12 | 0 | 0 | 0 | 18578 | 18577.72 | 65954 | 1.00 | 1.00 | 40.04 | 40.04 | 見合う | 60.54 | 60.54 | 見合う |
| 120 | 12 | 0 | 0 | 0 | 14978 | 14977.72 | 62354 | 1.00 | 1.00 | 32.28 | 32.28 | 見合う | 48.81 | 48.81 | 見合う |

#### answer:stalled（e = 0 の区間 20 件、e_r(A) は出ない・消さない）

| e 分 | n | 未終了（下限 > e） | 未終了（下限 ≦ e、除く） | 戻ったか不明 | R⁻ 秒 | R⁺ 秒 | R の 90% 秒 | p⁺ | p⁻ | M⁻(I) | M⁺(I) | I | M⁻(A) | M⁺(A) | A |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 20 | 0 | 0 | 0 | 219 | 218.87 | 597 | 1.00 | 1.00 | 0.47 | 0.47 | 見合わない | 0.71 | 0.71 | 見合わない |
| 5 | 5 | 0 | 0 | 0 | 278 | 277.87 | 977 | 1.00 | 1.00 | 0.60 | 0.60 | 見合わない | 0.91 | 0.91 | 見合わない |
| 10 | 2 | 0 | 0 | 0 | 593 | 592.64 | 929 | 1.00 | 1.00 | 1.28 | 1.28 | 見合わない | 1.93 | 1.93 | 見合わない |
| 15 | 1 | 0 | 0 | 0 | 713 | 713.11 | 713 | 1.00 | 1.00 | 1.54 | 1.54 | 見合わない | 2.32 | 2.32 | 見合う |
| 30 | 0 | 0 | 0 | — | — | — | — | — | — | — | — | — | — | — | — |
| 60 | 0 | 0 | 0 | — | — | — | — | — | — | — | — | — | — | — | — |
| 120 | 0 | 0 | 0 | — | — | — | — | — | — | — | — | — | — | — | — |

#### other:returning（e = 0 の区間 20 件、e_r(A) = 0 分）

| e 分 | n | 未終了（下限 > e） | 未終了（下限 ≦ e、除く） | 戻ったか不明 | R⁻ 秒 | R⁺ 秒 | R の 90% 秒 | p⁺ | p⁻ | M⁻(I) | M⁺(I) | I | M⁻(A) | M⁺(A) | A |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 20 | 0 | 0 | 0 | 6618 | 6618.06 | 32191 | 1.00 | 1.00 | 14.26 | 14.26 | 見合う | 21.57 | 21.57 | 見合う |
| 5 | 16 | 0 | 0 | 0 | 12991 | 12990.64 | 50645 | 1.00 | 1.00 | 28.00 | 28.00 | 見合う | 42.33 | 42.33 | 見合う |
| 10 | 16 | 0 | 0 | 0 | 12691 | 12690.64 | 50345 | 1.00 | 1.00 | 27.35 | 27.35 | 見合う | 41.35 | 41.35 | 見合う |
| 15 | 16 | 0 | 0 | 0 | 12391 | 12390.64 | 50045 | 1.00 | 1.00 | 26.70 | 26.70 | 見合う | 40.38 | 40.38 | 見合う |
| 30 | 14 | 0 | 0 | 0 | 17258 | 17258.22 | 58522 | 1.00 | 1.00 | 37.19 | 37.19 | 見合う | 56.24 | 56.24 | 見合う |
| 60 | 12 | 0 | 0 | 0 | 19037 | 19037.12 | 66099 | 1.00 | 1.00 | 41.03 | 41.03 | 見合う | 62.03 | 62.03 | 見合う |
| 120 | 9 | 0 | 0 | 0 | 17340 | 17340.21 | 70530 | 1.00 | 1.00 | 37.37 | 37.37 | 見合う | 56.50 | 56.50 | 見合う |

#### resume_after_limit（e = 0 の区間 11 件、e_r(A) = 0 分）

| e 分 | n | 未終了（下限 > e） | 未終了（下限 ≦ e、除く） | 戻ったか不明 | R⁻ 秒 | R⁺ 秒 | R の 90% 秒 | p⁺ | p⁻ | M⁻(I) | M⁺(I) | I | M⁻(A) | M⁺(A) | A |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 11 | 0 | 0 | 0 | 1682 | 1681.53 | 7845 | 1.00 | 1.00 | 3.62 | 3.62 | 見合う | 5.48 | 5.48 | 見合う |
| 5 | 9 | 0 | 0 | 0 | 1609 | 1608.82 | 13610 | 1.00 | 1.00 | 3.47 | 3.47 | 見合う | 5.24 | 5.24 | 見合う |
| 10 | 8 | 0 | 0 | 0 | 1326 | 1326.19 | 16343 | 1.00 | 1.00 | 2.86 | 2.86 | 見合う | 4.32 | 4.32 | 見合う |
| 15 | 7 | 0 | 0 | 0 | 1044 | 1043.56 | 19076 | 1.00 | 1.00 | 2.25 | 2.25 | 見合う | 3.40 | 3.40 | 見合う |
| 30 | 5 | 0 | 0 | 0 | 5390 | 5390.40 | 24242 | 1.00 | 1.00 | 11.62 | 11.62 | 見合う | 17.56 | 17.56 | 見合う |
| 60 | 3 | 0 | 0 | 0 | 4245 | 4244.75 | 28508 | 1.00 | 1.00 | 9.15 | 9.15 | 見合う | 13.83 | 13.83 | 見合う |
| 120 | 2 | 0 | 0 | 0 | 15809 | 15809.02 | 27940 | 1.00 | 1.00 | 34.07 | 34.07 | 見合う | 51.51 | 51.51 | 見合う |

#### other:held（e = 0 の区間 10 件、e_r(A) = 0 分）

| e 分 | n | 未終了（下限 > e） | 未終了（下限 ≦ e、除く） | 戻ったか不明 | R⁻ 秒 | R⁺ 秒 | R の 90% 秒 | p⁺ | p⁻ | M⁻(I) | M⁺(I) | I | M⁻(A) | M⁺(A) | A |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 10 | 1 | 0 | 2 | 7530 | 7530.25 | 52387 | 1.00 | 0.80 | 16.23 | 20.29 | 見合う | 24.54 | 30.67 | 見合う |
| 5 | 9 | 1 | 0 | 2 | 8380 | 8379.93 | 69548 | 1.00 | 0.78 | 18.06 | 23.22 | 見合う | 27.31 | 35.11 | 見合う |
| 10 | 9 | 1 | 0 | 2 | 8080 | 8079.93 | 69248 | 1.00 | 0.78 | 17.41 | 22.39 | 見合う | 26.33 | 33.85 | 見合う |
| 15 | 9 | 1 | 0 | 2 | 7780 | 7779.93 | 68948 | 1.00 | 0.78 | 16.77 | 21.56 | 見合う | 25.35 | 32.59 | 見合う |
| 30 | 7 | 1 | 0 | 2 | 9799 | 9798.75 | 102969 | 1.00 | 0.71 | 21.12 | 29.57 | 見合う | 31.93 | 44.70 | 見合う |
| 60 | 6 | 1 | 0 | 2 | 14832 | 14831.83 | 118630 | 1.00 | 0.67 | 31.97 | 47.95 | 見合う | 48.33 | 72.50 | 見合う |
| 120 | 5 | 1 | 0 | 2 | 18065 | 18064.91 | 132490 | 1.00 | 0.60 | 38.93 | 64.89 | 見合う | 58.87 | 98.11 | 見合う |

#### other:review_held（e = 0 の区間 2 件、e_r(A) は unresolved）

| e 分 | n | 未終了（下限 > e） | 未終了（下限 ≦ e、除く） | 戻ったか不明 | R⁻ 秒 | R⁺ 秒 | R の 90% 秒 | p⁺ | p⁻ | M⁻(I) | M⁺(I) | I | M⁻(A) | M⁺(A) | A |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 2 | 0 | 0 | 0 | 11 | 11.26 | 13 | 1.00 | 1.00 | 0.02 | 0.02 | 見合わない | 0.04 | 0.04 | 見合わない |
| 5 | 0 | 0 | 0 | — | — | — | — | — | — | — | — | — | — | — | — |
| 10 | 0 | 0 | 0 | — | — | — | — | — | — | — | — | — | — | — | — |
| 15 | 0 | 0 | 0 | — | — | — | — | — | — | — | — | — | — | — | — |
| 30 | 0 | 0 | 0 | — | — | — | — | — | — | — | — | — | — | — | — |
| 60 | 0 | 0 | 0 | — | — | — | — | — | — | — | — | — | — | — | — |
| 120 | 0 | 0 | 0 | — | — | — | — | — | — | — | — | — | — | — | — |

#### answer:blocked（e = 0 の区間 2 件、e_r(A) は unresolved）

| e 分 | n | 未終了（下限 > e） | 未終了（下限 ≦ e、除く） | 戻ったか不明 | R⁻ 秒 | R⁺ 秒 | R の 90% 秒 | p⁺ | p⁻ | M⁻(I) | M⁺(I) | I | M⁻(A) | M⁺(A) | A |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 2 | 0 | 0 | 0 | 7082 | 7082.38 | 12543 | 0.50 | 0.50 | 30.53 | 30.53 | 見合う | 46.16 | 46.16 | 見合う |
| 5 | 1 | 0 | 0 | 0 | 13609 | 13608.61 | 13609 | 0.00 | 0.00 | ∞ | ∞ | 見合う | ∞ | ∞ | 見合う |
| 10 | 1 | 0 | 0 | 0 | 13309 | 13308.61 | 13309 | 0.00 | 0.00 | ∞ | ∞ | 見合う | ∞ | ∞ | 見合う |
| 15 | 1 | 0 | 0 | 0 | 13009 | 13008.61 | 13009 | 0.00 | 0.00 | ∞ | ∞ | 見合う | ∞ | ∞ | 見合う |
| 30 | 1 | 0 | 0 | 0 | 12109 | 12108.61 | 12109 | 0.00 | 0.00 | ∞ | ∞ | 見合う | ∞ | ∞ | 見合う |
| 60 | 1 | 0 | 0 | 0 | 10309 | 10308.61 | 10309 | 0.00 | 0.00 | ∞ | ∞ | 見合う | ∞ | ∞ | 見合う |
| 120 | 1 | 0 | 0 | 0 | 6709 | 6708.61 | 6709 | 0.00 | 0.00 | ∞ | ∞ | 見合う | ∞ | ∞ | 見合う |

### e_r と種類ごとの推奨

| 種類 r | e = 0 の区間 | e_r(I) | e_r(A) | 推奨 | 今の形（e = 0 で A）の M⁻_r(0, A) | Δt を 674 秒（Q0 の resume の差）にしたときの M_r(0, A) |
|---|---|---|---|---|---|---|
| landing_slot | 785 | unresolved（Q1） | 0 分 | e = 0 で A（ただし下の着地の検証の代償に注意） | 136.56 | 62.51 |
| needs_session_other | 204 | unresolved（Q1） | 0 分 | e = 0 で A | 16.77 | 7.68 |
| answer:approve_landing | 183 | unresolved（Q1） | 0 分 | e = 0 で A | 19.06 | 8.72 |
| landing_answer_no_ask | 175 | unresolved（Q1） | 0 分 | e = 0 で A | 17.88 | 8.18 |
| other:awaiting_e2e | 90 | unresolved（Q1） | unresolved（90 区間は task 4209・4844・5059 の 3 本の run で `awaiting_slot` と `awaiting_e2e` が約 2〜17 秒ごとに行き来した断片で、run の数では標本不足。表の機械的な値は e = 0 で M = ∞） | 決まらない | ∞ | ∞ |
| answer:worker_question | 25 | unresolved（Q1） | 0 分 | e = 0 で A | 6.19 | 2.83 |
| waiting_no_ask | 23 | unresolved（Q1） | 0 分 | e = 0 で A | 35.26 | 16.14 |
| answer:stalled | 20 | unresolved（Q1） | 出ない | 消さない（n ≧ 10 の e = 0 で見合わないに決まり、e ≧ 5 は n < 10） | 0.71 | 0.33 |
| other:returning | 20 | unresolved（Q1） | 0 分 | e = 0 で A | 21.57 | 9.87 |
| resume_after_limit | 11 | unresolved（Q1） | 0 分 | e = 0 で A | 5.48 | 2.51 |
| other:held | 10 | unresolved（Q1） | 0 分 | e = 0 で A | 24.54 | 11.23 |
| answer:blocked | 2 | unresolved（Q1） | unresolved（標本不足） | 決まらない | 46.16 | 21.13 |
| other:review_held | 2 | unresolved（Q1） | unresolved（標本不足） | 決まらない | 0.04 | 0.02 |

- 同じ e で M⁻ は A が I より大きい（G(A) > G(I) かつ Δt(A) < Δt(I)）ので、出た種類の消す対象は A。
- ask の区間は答えで終わるので、R は答えまでの時間だけを数える。答えの後の resume の前の待ちは、needs_session_other・waiting_no_ask・returning の区間に分かれて入る。
- stalled は ask の区間の中央値が 176 秒と短く、e = 0 でも見合わない（M⁻ = 0.71）。今の通常の掃除の条件（開いている ask があれば消す）は stalled の待ちにも当たるので、今の形は stalled では見合わない（観測区間に stalled の ask で消した行は無かった）。
- landing_slot の 785 区間のうち 90 区間は、上の 3 本の run で e2e の待ちから戻るたびに始まり直した断片。これを除くと e = 0 で 695 区間、R の中央値 1433 秒、p = 0.029、M_r(0, A) = 162.25 で、結論は変わらない。
- 着地 slot の待ちは戻る割合 p が小さく（e ≦ 30 分で 0.03〜0.05）、M が大きくなる。M は worker が戻る代償だけを数え、着地の検証や e2e が消した target を作り直す代償（Spike 3440 で incremental だけで約 1.8 倍、全部で約 2.2 倍。この task の description が引く値）を入れていない。landing_slot で見合うかは着地の検証の延びとの比べが決める。

### 参考：起点を区間の始まりにしたとき

ask の無い区間の起点（直前の turn_finished）から区間の始まりまでには、検証・review・前の答えの待ちが入る（上の「待ちの区間と種類」）。
起点をその種類の待ちに入った時刻（区間の始まり）に置き換えて同じ表を作った値を、判定に使わない参考として並べる。

| 種類 r | 指定の起点の e = 0 の区間 | 指定の起点の e_r(A) | 指定の起点の M⁻_r(0, A) | 区間の始まりを起点にした e = 0 の区間 | その e_r(A) | その M⁻_r(0, A) |
|---|---|---|---|---|---|---|
| landing_slot | 785 | 0 分 | 136.56 | 786 | 0 分 | 6.33 |
| needs_session_other | 204 | 0 分 | 16.77 | 206 | 0 分 | 2.30 |
| answer:approve_landing | 183 | 0 分 | 19.06 | 185 | 0 分 | 15.81 |
| landing_answer_no_ask | 175 | 0 分 | 17.88 | 177 | 5 分 | 0.03 |
| other:awaiting_e2e | 90 | unresolved（3 本の run） | ∞ | 90 | unresolved（3 本の run） | ∞ |
| answer:worker_question | 25 | 0 分 | 6.19 | 25 | 0 分 | 6.19 |
| waiting_no_ask | 23 | 0 分 | 35.26 | 32 | 5 分 | 0.16 |
| answer:stalled | 20 | 消さない | 0.71 | 20 | 消さない | 0.57 |
| other:returning | 20 | 0 分 | 21.57 | 20 | 5 分 | 1.33 |
| resume_after_limit | 11 | 0 分 | 5.48 | 11 | 消さない | 1.48 |
| other:held | 10 | 0 分 | 24.54 | 10 | 0 分 | 7.49 |
| answer:blocked | 2 | unresolved | 46.16 | 2 | unresolved | 46.16 |
| other:review_held | 2 | unresolved | 0.04 | 2 | unresolved | 0.02 |

ask の区間（approve_landing・worker_question・stalled）は起点がほぼ同じなので変わらない。
変わるのは ask の無い landing_answer・waiting と returning（0 分 → 5 分）と、上限で止まった後の resume の待ち（0 分 → 消さない）。
どれも、区間そのものは短く（ask の無い landing_answer は中央値 6 秒、waiting_no_ask は 8 秒）、指定の起点では区間の前の時間が R に入って M が大きく出る。
runtime が待ちの始まりとして読めるのはどちらの時刻か（掃除の候補は turn の後に何も動いていないことを `turn_finished` で見る）で、実装の task が起点を選ぶ。

## disk_space の消し方の除外

disk_space の 251 件を、description の 2 つの「除く対象」で数えた。

- 消した後 10 分以内に待ちが終わった（答え・resume・着地の始まりまで）：真 108 件、偽 120 件、不明 23 件（削除の時刻の範囲で 600 秒をまたぐ行と、種類・待ちの決まらない行）。
- 消した後の最初の turn がまた消されたか上限で止まった：真 44 件、偽 205 件、不明 2 件（次の turn が無いと決まった行は偽）。
- どちらかに当たる（除く対象）：真 138 件、偽 96 件、不明 17 件。
- 保留が 10 分で解けた：真 47 件、偽 183 件、不明 21 件（保留が無いと決まった行は偽）。

description の 2 通りの置き方：S1 は不明の除く対象を全部当たる・不明の保留を全部解けたとし、条件の当てはめが決まらない行（e の範囲が閾値をまたぐ行など）を除く側に置く。S2 はその逆。

| 置き方 | 条件 | 除く件 | うち除く対象 | 除く対象で残る件 | 保留の解けた件で残す | 保留の解けた件で除く |
|---|---|---|---|---|---|---|
| S1 | C1 e_r(A) を disk_space にも当てる（e_r の出ない種類・種類の決まらない行・待ちの外は消さない） | 10 | 10 | 145 | 65 | 3 |
| S1 | C2 turn の上限で止まった後の resume の待ちを除く | 4 | 3 | 152 | 68 | 0 |
| S1 | C3 needs_session の待ち（resume の直前）を全部除く | 22 | 18 | 137 | 64 | 4 |
| S1 | C4 待ちに入って 10 分以上の run だけ消す（待ちの長い順の代わり） | 184 | 115 | 40 | 16 | 52 |
| S1 | C5 C3 かつ e ≧ 5 分 | 150 | 97 | 58 | 24 | 44 |
| S2 | C1 e_r(A) を disk_space にも当てる（e_r の出ない種類・種類の決まらない行・待ちの外は消さない） | 10 | 5 | 133 | 45 | 2 |
| S2 | C2 turn の上限で止まった後の resume の待ちを除く | 4 | 3 | 135 | 47 | 0 |
| S2 | C3 needs_session の待ち（resume の直前）を全部除く | 17 | 12 | 126 | 46 | 1 |
| S2 | C4 待ちに入って 10 分以上の run だけ消す（待ちの長い順の代わり） | 156 | 91 | 47 | 11 | 36 |
| S2 | C5 C3 かつ e ≧ 5 分 | 116 | 71 | 67 | 21 | 26 |

推奨は **C2（turn の上限で止まった後の resume の待ちを除く）**。description の選び方（保留の解けた件を全部残す条件のうち、除く対象を最も多く除くもの）で、保留の解けた件を 1 件も除かない条件は S1・S2 とも C2 だけだった。C2 が除くのは 4 件で、除く対象は 3 件（task 4945・4133 の 00:39:11 は 10 分以内に待ちが終わり、4312 は次の turn の後にまた消された）。2 通りとも同じなので、この推奨は決まった。

C2 が除く対象を 3 件しか除かないのは、除く対象（138〜155 件）のうち 111〜121 件が着地 slot の待ちで、消す時点の種類と経過では消してから 10 分以内に着地が始まる件と見分けられないから。経過で絞る C4・C5 は除く対象を多く除くが、保留の解けた件も多く除く（S2 で 47 件中 26〜36 件）。

taskの基準による結論とは別の提案：C1（e_r(A) を disk_space にも当て、e_r の出ない種類・種類の決まらない行・待ちの外は消さない）は、除く対象を 5〜10 件除くが、保留の解けた件を 2〜3 件除くので、description の選び方では選ばれない。この比べは planner の判断の材料として残す。

disk_space の 251 件のうち、どれかの条件で除くか当てはめが決まらない行と、除く対象か保留の解けた件に当たる（か不明の）行の 232 件（残りの 19 件はどの条件でも残し、除く対象にも保留の解けた件にも当たらないと決まった行）：

<details>
<summary>232 件の表</summary>

| 記録の時刻（UTC） | task | 種類 r | e 秒 | 10 分以内に待ちが終わった | また消された・上限 | 保留が 10 分で解けた | C1 | C2 | C3 | C4 | C5 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-11-02T18:48:29 | 3946 | landing_slot | 380〜384 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-03T07:41:20 | 3844 | unknown:landing_answer_no_ask|needs_session_other | — | 不明 | 真 | 偽 | 除く | 残す | 不明 | 不明 | 不明 |
| 2026-11-03T13:23:17 | 4016 | landing_slot | 3805〜3810 | 真 | 偽 | 真 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-03T18:10:37 | 3976 | answer:worker_question | 173〜5854 | 偽 | 真 | 偽 | 残す | 残す | 残す | 不明 | 不明 |
| 2026-11-03T22:30:47 | 4094 | landing_slot | 932〜1101 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-04T00:03:12 | 3976 | answer:worker_question | 4〜206 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-04T02:47:39 | 4081 | landing_slot | 8872〜8943 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-04T03:23:34 | 3976 | landing_slot | 42〜111 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-04T23:23:51 | 3887 | landing_slot | 716〜1813 | 偽 | 偽 | 不明 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-05T16:59:23 | 3978 | answer:worker_question | 19〜4059 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 不明 | 不明 |
| 2026-11-06T02:52:43 | 4084 | landing_answer_no_ask | 2702〜5477 | 不明 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-06T12:12:56 | 4099 | answer:worker_question | 49〜55 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-06T13:49:41 | 4096 | landing_slot | 326〜455 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-06T14:25:03 | 4168 | unknown:landing_answer_no_ask|needs_session_other | — | 不明 | 偽 | 偽 | 除く | 残す | 不明 | 不明 | 不明 |
| 2026-11-06T16:01:10 | 4100 | unknown:landing_slot|needs_session_other | — | 不明 | 真 | 偽 | 除く | 残す | 不明 | 不明 | 不明 |
| 2026-11-06T17:11:57 | 4100 | landing_slot | 1521〜1828 | 偽 | 真 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-06T18:16:08 | 4100 | landing_slot | 290〜297 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-06T18:39:21 | 4105 | landing_slot | 463〜473 | 偽 | 偽 | 不明 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-06T19:37:08 | 4251 | landing_slot | 310〜424 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-06T22:01:06 | 4251 | landing_slot | 701〜769 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-06T22:37:01 | 4086 | landing_slot | 740〜1273 | 偽 | 偽 | 不明 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-06T23:31:25 | 4093 | unknown:landing_answer_no_ask|needs_session_other | — | 不明 | 真 | 偽 | 除く | 残す | 不明 | 不明 | 不明 |
| 2026-11-06T23:55:51 | 4098 | landing_slot | 221〜228 | 真 | 真 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-07T00:11:34 | 4098 | landing_slot | 484〜1173 | 不明 | 真 | 偽 | 残す | 残す | 残す | 不明 | 残す |
| 2026-11-07T00:33:28 | 4098 | landing_slot | 10〜25 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-07T00:40:07 | 4098 | landing_slot | 297〜424 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-07T02:42:20 | 4450 | landing_slot | 415〜990 | 偽 | 真 | 不明 | 残す | 残す | 残す | 不明 | 残す |
| 2026-11-07T04:16:20 | 4093 | answer:worker_question | 557〜752 | 偽 | 偽 | 真 | 残す | 残す | 残す | 不明 | 残す |
| 2026-11-07T04:52:45 | 4450 | landing_slot | 466〜539 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-07T08:45:46 | 4441 | answer:worker_question | 2〜44 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-07T10:47:57 | 4297 | landing_slot | 143〜200 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-07T10:54:54 | 4237 | landing_slot | 148〜309 | 偽 | 偽 | 不明 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-07T12:33:15 | 4250 | landing_slot | 81〜101 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-07T12:37:34 | 4294 | landing_slot | 104〜148 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-07T13:13:48 | 4305 | landing_slot | 1269〜1392 | 偽 | 真 | 不明 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-07T14:05:13 | 4043 | landing_slot | 390〜549 | 偽 | 偽 | 不明 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-07T16:42:46 | 4138 | landing_slot | 621〜1050 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-07T17:52:59 | 4178 | landing_slot | 278〜579 | 不明 | 偽 | 不明 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-07T18:28:34 | 4205 | landing_slot | 753〜2523 | 不明 | 偽 | 不明 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-07T18:47:55 | 4206 | landing_slot | 517〜1346 | 不明 | 偽 | 偽 | 残す | 残す | 残す | 不明 | 残す |
| 2026-11-07T19:45:37 | 4209 | landing_slot | 174〜178 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-07T22:01:42 | 4214 | landing_slot | 590〜845 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 不明 | 残す |
| 2026-11-07T22:01:42 | 4212 | landing_slot | 1418〜1672 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-07T23:58:36 | 4213 | landing_slot | 1463〜1715 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-08T00:33:52 | 4216 | landing_slot | 465〜1275 | 不明 | 偽 | 不明 | 残す | 残す | 残す | 不明 | 残す |
| 2026-11-08T00:33:52 | 4218 | landing_slot | 303〜1448 | 不明 | 偽 | 不明 | 残す | 残す | 残す | 不明 | 残す |
| 2026-11-08T04:52:34 | 4224 | landing_slot | 1559〜1782 | 不明 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-08T08:42:39 | 4452 | landing_slot | 188〜665 | 不明 | 偽 | 偽 | 残す | 残す | 残す | 不明 | 不明 |
| 2026-11-08T09:27:34 | 4471 | landing_slot | 75〜89 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-08T10:34:40 | 4093 | needs_session_other | 198〜224 | 偽 | 真 | 偽 | 残す | 残す | 除く | 除く | 除く |
| 2026-11-08T10:46:44 | 4093 | needs_session_other | 198〜948 | 偽 | 真 | 偽 | 残す | 残す | 除く | 不明 | 除く |
| 2026-11-08T11:59:26 | 4484 | landing_slot | 78〜124 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-08T14:39:11 | 4448 | landing_slot | 177〜584 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-08T15:32:06 | 4473 | landing_slot | 238〜299 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-08T15:44:14 | 4589 | landing_slot | 106〜171 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-08T16:52:35 | 4182 | landing_slot | 165〜223 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-08T17:51:13 | 4305 | landing_slot | 219〜287 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-08T17:58:49 | 4305 | landing_slot | 670〜743 | 真 | 偽 | 真 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-08T18:07:29 | 4597 | landing_slot | 100〜106 | 偽 | 真 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-08T18:50:06 | 4597 | landing_slot | 139〜170 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-08T20:11:52 | 4361 | landing_slot | 187〜504 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-08T20:15:18 | 4361 | landing_slot | 704〜709 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-08T20:50:20 | 4591 | landing_slot | 277〜299 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-08T21:34:17 | 4598 | landing_slot | 327〜477 | 不明 | 真 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-08T22:08:57 | 4106 | landing_slot | 311〜363 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-08T22:09:59 | 4598 | landing_slot | 381〜396 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-08T23:07:13 | 4362 | landing_slot | 126〜1493 | 偽 | 偽 | 不明 | 残す | 残す | 残す | 不明 | 不明 |
| 2026-11-08T23:59:27 | 4474 | landing_slot | 181〜328 | 不明 | 偽 | 真 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-09T00:26:38 | 4542 | landing_slot | 303〜306 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-09T00:31:54 | 4542 | landing_slot | 615〜623 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-09T01:06:10 | 4590 | landing_slot | 3〜1526 | 偽 | 偽 | 不明 | 残す | 残す | 残す | 不明 | 不明 |
| 2026-11-09T02:39:56 | 4071 | landing_slot | 189〜228 | 真 | 真 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T03:21:42 | 4093 | landing_slot | 6852〜6898 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-09T03:34:13 | 4309 | landing_slot | 165〜230 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T04:30:10 | 4616 | landing_slot | 207〜223 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T05:48:59 | 4071 | landing_slot | 4〜22 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T06:55:05 | 4073 | landing_slot | 153〜202 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T07:11:54 | 4639 | landing_slot | 129〜192 | 真 | 真 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T07:42:18 | 4639 | needs_session_other | 1988〜2011 | 真 | 真 | 不明 | 残す | 残す | 除く | 残す | 除く |
| 2026-11-09T07:59:14 | 4617 | landing_slot | 100〜138 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T08:00:21 | 4639 | landing_slot | 68〜95 | 不明 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T08:15:53 | 4072 | landing_slot | 209〜250 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T10:03:30 | 4318 | landing_slot | 221〜256 | 真 | 真 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T10:34:03 | 4135 | landing_slot | 168〜212 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T10:41:45 | 4318 | landing_slot | 208〜234 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T11:02:32 | 4644 | landing_slot | 113〜165 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T12:44:24 | 4658 | landing_slot | 190〜238 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T14:41:32 | 4662 | landing_slot | 206〜249 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T15:11:39 | 4670 | landing_slot | 149〜188 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T16:25:31 | 4676 | landing_slot | 216〜225 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T17:37:29 | 4654 | landing_slot | 118〜183 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T17:37:29 | 4653 | landing_slot | 203〜218 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T17:47:34 | 4660 | answer:worker_question | 1〜49 | 偽 | 真 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T19:59:49 | 4093 | landing_slot | 283〜300 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-09T20:06:37 | 4319 | landing_slot | 100〜130 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T21:06:42 | 4261 | landing_slot | 212〜243 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T22:14:46 | 4145 | landing_slot | 91〜99 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-09T22:34:20 | 4298 | landing_slot | 374〜669 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 不明 | 残す |
| 2026-11-09T22:57:21 | 4653 | landing_slot | 337〜346 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-09T23:59:10 | 4655 | landing_slot | 347〜381 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-10T00:26:06 | 4071 | landing_slot | 242〜281 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-10T02:34:09 | 4260 | landing_slot | 104〜168 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-10T03:45:17 | 4299 | landing_slot | 109〜122 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-10T04:37:48 | 3949 | landing_slot | 146〜172 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-10T06:02:13 | 4385 | landing_slot | 311〜334 | 真 | 真 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-10T06:11:03 | 3950 | landing_slot | 148〜191 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-10T10:10:03 | 3951 | landing_slot | 235〜260 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-10T10:34:28 | 4074 | landing_slot | 443〜1081 | 偽 | 真 | 偽 | 残す | 残す | 残す | 不明 | 残す |
| 2026-11-10T11:57:24 | 4074 | needs_session_other | 6031〜6055 | 真 | 真 | 偽 | 残す | 残す | 除く | 残す | 除く |
| 2026-11-10T13:32:46 | 4074 | landing_slot | 887〜957 | 偽 | 真 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-10T15:06:06 | 4074 | landing_slot | 425〜449 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-10T16:46:08 | 4688 | landing_slot | 382〜409 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-11T00:03:11 | 4334 | landing_slot | 246〜252 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T00:33:53 | 4541 | landing_slot | 116〜179 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T01:24:30 | 4727 | unknown:landing_answer_no_ask|landing_slot | — | 不明 | 偽 | 不明 | 除く | 残す | 残す | 不明 | 不明 |
| 2026-11-11T01:40:29 | 4678 | landing_slot | 168〜216 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T01:41:35 | 3954 | landing_slot | 250〜274 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T03:19:21 | 4175 | needs_session_other | 3〜42 | 偽 | 不明 | 偽 | 残す | 残す | 除く | 除く | 除く |
| 2026-11-11T04:29:31 | 4379 | landing_slot | 230〜281 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T04:39:10 | 4312 | resume_after_limit | 3〜50 | 偽 | 真 | 偽 | 残す | 除く | 除く | 除く | 除く |
| 2026-11-11T04:55:25 | 4139 | landing_slot | 266〜321 | 偽 | 真 | 偽 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-11T05:47:00 | 4139 | landing_slot | 129〜191 | 真 | 真 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T06:33:53 | 4139 | needs_session_other | 2954〜3003 | 偽 | 真 | 偽 | 残す | 残す | 除く | 残す | 除く |
| 2026-11-11T06:58:09 | 4332 | landing_slot | 102〜162 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T07:53:47 | 4139 | landing_slot | 289〜319 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-11T08:05:24 | 4312 | landing_slot | 354〜413 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-11T08:18:09 | 4312 | landing_slot | 1131〜1178 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-11T08:51:38 | 4646 | landing_slot | 281〜297 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T10:58:25 | 4665 | landing_slot | 267〜302 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-11T12:35:58 | 4755 | unknown:landing_answer_no_ask|landing_slot | — | 不明 | 偽 | 真 | 除く | 残す | 残す | 不明 | 不明 |
| 2026-11-11T12:45:22 | 4667 | landing_slot | 152〜184 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T12:55:39 | 4669 | landing_slot | 70〜110 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T15:57:54 | 4792 | landing_slot | 119〜144 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T18:33:56 | 4677 | needs_session_other | 1092〜1100 | 真 | 真 | 真 | 残す | 残す | 除く | 残す | 除く |
| 2026-11-11T18:52:08 | 4677 | landing_slot | 138〜176 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T20:12:36 | 4729 | landing_slot | 115〜170 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T20:41:12 | 4772 | landing_slot | 178〜421 | 不明 | 偽 | 不明 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-11T20:58:05 | 4756 | landing_slot | 169〜194 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T21:16:45 | 4704 | landing_slot | 52〜463 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-11T21:26:39 | 4709 | landing_slot | 91〜188 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T23:44:47 | 4666 | landing_slot | 149〜154 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-11T23:47:09 | 4776 | landing_slot | 146〜152 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-12T01:16:17 | 4811 | landing_slot | 734〜748 | 真 | 偽 | 不明 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-12T01:44:00 | 4819 | landing_slot | 1631〜1884 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-12T01:53:57 | 4790 | landing_slot | 676〜683 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-12T01:57:58 | 4791 | landing_slot | 144〜150 | 偽 | 真 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-12T02:41:22 | 4663 | landing_slot | 213〜220 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-12T03:07:10 | 4791 | landing_slot | 125〜135 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-12T03:27:48 | 4823 | landing_slot | 107〜1306 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 不明 | 不明 |
| 2026-11-12T05:30:45 | 4132 | landing_slot | 184〜191 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-12T08:23:26 | 4793 | landing_slot | 347〜369 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-12T08:40:37 | 4806 | landing_slot | 563〜580 | 偽 | 真 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-12T09:12:35 | 4834 | landing_slot | 390〜396 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-12T09:19:12 | 4834 | landing_slot | 732〜792 | 真 | 偽 | 真 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-12T09:42:51 | 4312 | landing_slot | 445〜452 | 真 | 真 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-12T09:45:36 | 4806 | landing_slot | 421〜443 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-12T09:50:05 | 4312 | landing_slot | 773〜886 | 真 | 真 | 真 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-12T09:56:58 | 4835 | landing_slot | 60〜70 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-12T10:08:34 | 4836 | landing_slot | 99〜218 | 偽 | 偽 | 不明 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-12T13:22:12 | 4312 | needs_session_other | 3443〜13613 | 偽 | 真 | 不明 | 残す | 残す | 除く | 残す | 除く |
| 2026-11-12T19:36:02 | 4625 | landing_slot | 180〜208 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-12T19:39:22 | 4625 | landing_slot | 384〜408 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-12T20:13:55 | 4842 | landing_slot | 308〜325 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-12T23:14:02 | 4122 | landing_slot | 302〜334 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-12T23:34:22 | 4843 | landing_slot | 158〜206 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-12T23:40:09 | 4843 | landing_slot | 541〜553 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-13T03:43:39 | 4624 | landing_slot | 217〜245 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-13T03:52:28 | 4627 | needs_session_other | 3349〜3357 | 偽 | 真 | 偽 | 残す | 残す | 除く | 残す | 除く |
| 2026-11-13T03:59:20 | 4624 | landing_slot | 1144〜1186 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-13T04:28:18 | 4629 | landing_slot | 13〜44 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-13T05:27:58 | 4627 | landing_slot | 192〜233 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-13T06:31:20 | 4845 | unknown:other:held|other:review_held | — | 不明 | 真 | 真 | 除く | 残す | 残す | 不明 | 不明 |
| 2026-11-13T06:50:31 | 4807 | answer:worker_question | 4〜81 | 偽 | 真 | 不明 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-13T11:13:19 | 4638 | landing_slot | 367〜417 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-13T11:51:13 | 4844 | landing_slot | 354〜389 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-13T11:58:53 | 4808 | landing_slot | 579〜643 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 不明 | 残す |
| 2026-11-13T12:22:54 | 4638 | needs_session_other | 4577〜4592 | 偽 | 偽 | 偽 | 残す | 残す | 除く | 残す | 除く |
| 2026-11-13T13:31:03 | 4807 | unknown:other:held|other:review_held | — | 不明 | 真 | 偽 | 除く | 残す | 残す | 不明 | 不明 |
| 2026-11-13T14:55:20 | 4807 | landing_slot | 200〜245 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-13T16:36:38 | 4849 | answer:worker_question | 1〜41 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-13T16:52:42 | 3162 | landing_slot | 427〜481 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-13T23:16:23 | 4849 | landing_slot | 812〜820 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-13T23:35:44 | 4133 | answer:worker_question | 14〜106 | 真 | 真 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-14T00:39:11 | 4133 | resume_after_limit | 5〜16 | 真 | 偽 | 偽 | 残す | 除く | 除く | 除く | 除く |
| 2026-11-14T01:22:50 | 4849 | landing_answer_no_ask | 8309〜8409 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-14T01:28:27 | 4844 | landing_slot | 10988〜11000 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-14T02:15:16 | 4844 | needs_session_other | 13740〜13810 | 偽 | 偽 | 偽 | 残す | 残す | 除く | 残す | 除く |
| 2026-11-14T03:22:55 | 4844 | landing_slot | 287〜359 | 偽 | 真 | 偽 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-14T04:52:06 | 4942 | landing_slot | 295〜381 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-14T06:02:38 | 4844 | landing_answer_no_ask | 9916〜9942 | 偽 | 真 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-15T06:43:55 | 4133 | landing_slot | 231〜308 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-15T07:07:47 | 4945 | resume_after_limit | 7〜24 | 真 | 偽 | 偽 | 残す | 除く | 除く | 除く | 除く |
| 2026-11-15T07:58:39 | 4945 | landing_slot | 257〜314 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-15T08:10:39 | 4943 | landing_slot | 122〜151 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-15T09:07:29 | 4844 | landing_slot | 574〜585 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-15T09:13:54 | 4844 | landing_slot | 895〜971 | 偽 | 偽 | 真 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-15T10:44:58 | 4945 | landing_slot | 176〜244 | 偽 | 真 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-15T12:05:50 | 4844 | landing_answer_no_ask | 11231〜11287 | 偽 | 偽 | 真 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-15T13:27:48 | 4979 | landing_slot | 217〜233 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-15T14:02:35 | 4845 | landing_slot | 281〜300 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-15T14:09:00 | 4845 | landing_slot | 668〜686 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-15T14:27:47 | 4945 | landing_slot | 154〜164 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-15T15:08:33 | 4845 | needs_session_other | 3601〜4259 | 偽 | 偽 | 不明 | 残す | 残す | 除く | 残す | 除く |
| 2026-11-15T16:02:28 | 4950 | landing_slot | 148〜155 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-15T16:09:06 | 4950 | landing_slot | 534〜552 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-15T17:03:49 | 4984 | landing_slot | 269〜278 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-15T17:22:15 | 4991 | landing_slot | 158〜520 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-15T17:52:45 | 4944 | landing_slot | 112〜158 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-15T20:08:25 | 4981 | landing_slot | 129〜140 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-15T21:45:23 | 4849 | answer:worker_question | 4〜51 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-15T23:04:38 | 4997 | landing_slot | 172〜234 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-15T23:42:29 | 4844 | landing_slot | 3672〜3726 | 真 | 真 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-16T02:45:06 | 4985 | answer:worker_question | 55〜80 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-16T06:43:20 | 4956 | landing_slot | 152〜185 | 真 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-16T08:28:22 | 5059 | landing_slot | 221〜240 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-16T08:30:58 | 4891 | landing_slot | 564〜626 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 不明 | 残す |
| 2026-11-16T09:11:29 | 4946 | landing_slot | 262〜283 | 偽 | 偽 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-16T09:48:06 | 4893 | landing_slot | 1759〜1780 | 偽 | 偽 | 真 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-16T10:34:51 | 4891 | unknown:landing_answer_no_ask|other:held | — | 不明 | 不明 | 偽 | 除く | 残す | 残す | 不明 | 不明 |
| 2026-11-16T10:49:44 | 4946 | landing_answer_no_ask | 6147〜6177 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-16T10:51:21 | 4906 | landing_slot | 5349〜5454 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-16T10:51:21 | 4897 | landing_slot | 5330〜5435 | 真 | 偽 | 偽 | 残す | 残す | 残す | 残す | 残す |
| 2026-11-16T11:03:34 | 4921 | landing_slot | 110〜130 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-16T12:10:55 | 5011 | answer:worker_question | 4〜56 | 真 | 真 | 真 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-16T12:15:00 | 5027 | answer:worker_question | 5〜243 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 除く |
| 2026-11-16T12:48:20 | 5060 | landing_slot | 426〜435 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-16T13:14:02 | 5011 | resume_after_limit | 5〜21 | 偽 | 偽 | 偽 | 残す | 除く | 除く | 除く | 除く |
| 2026-11-16T14:51:28 | 5064 | unknown:needs_session_other|other:held | — | 不明 | 偽 | 偽 | 除く | 残す | 不明 | 不明 | 不明 |
| 2026-11-16T15:17:52 | 5108 | landing_slot | 304〜324 | 偽 | 真 | 偽 | 残す | 残す | 残す | 除く | 残す |
| 2026-11-16T16:01:08 | 5108 | needs_session_other | 2873〜2920 | 偽 | 真 | 偽 | 残す | 残す | 除く | 残す | 除く |
| 2026-11-16T16:45:52 | 5108 | landing_slot | 274〜324 | 真 | 偽 | 偽 | 残す | 残す | 残す | 除く | 不明 |
| 2026-11-16T17:37:23 | 5064 | landing_slot | 416〜455 | 偽 | 偽 | 偽 | 残す | 残す | 残す | 除く | 残す |
</details>

## 次の turn の上限

消した後の最初の turn が上限で止まった割合は 1.4%（220 件中 3 件）、消していない revise・resume の turn は 1.3%（451 件中 6 件）。未終了の turn を両端に置いた差は −0.19〜+0.48 ポイントで、2 通りとも 10 ポイント未満。resume どうしでも −4.09〜−3.63 ポイントで、消した後のほうが低い。
推奨は **上限を変えない**（決まった）。4133 の 00:38 の時間切れは、resume_timeout が 3600 秒だった間の 1 件で、今の値は 7200 秒。

## 判定と根拠

### Q0

記録の値は決まった：消した後の最初の revise・resume の turn は中央値で 851 秒（未終了を両端に置いて 854 秒、resume どうしで 674 秒）長く、上限で止まった割合は差が無い（−0.19〜+0.48 ポイント）。悪循環は 3 件（決まらない行 3 件）。
中央値の差が 300 秒を超えるので、問い全体の does_not_hold の条件（差が無く今の形を保つのがよい）には当たらない。差には turn の中身の違いが入るので、作り直しの代償そのものは Q1 の Δt を使う。

### Q1

- I：description の基準は「両方が有効な対の延びの過半が Δt(I) と同じ側（300 秒以下か超え）になければ unresolved」。Δt(I) = 474 − 134.5 = 339.5 秒で 300 秒を超えるが、両方が有効な 4 対の延びは 374・296・287・402 秒で、300 秒を超えるのは 2 対だけで過半にならない。description の基準で **unresolved**（揺れで決まらない）。対ごとの比は 2.83〜4.22 で、延びは 287〜402 秒の幅にある。
- A：Δt(A) = 443 − 134.5 = 308.5 秒で 300 秒を超えるので **does_not_hold**。有効な回は 2 回（412・474 秒）で、比較は成立する（A は 1 回以上）。
- 延びの中身：lib の build が 60〜90 秒から 230〜350 秒、clippy の check が 45〜65 秒から 130〜195 秒になる。依存の crate は A でも sccache で戻るので、A の延びは I とほぼ同じで、消える incremental の作り直しが延びのほとんど。

### Q2

指定の起点で、e_r(A) が 9 種類で出た（どれも 0 分）。stalled は「消さない」が決まった（n ≧ 10 の e = 0 で見合わない）。review_held・blocked は標本不足、e2e の待ちは 3 本の run の断片で標本不足で決まらない。e_r(I) は Q1 が unresolved なので出さない。
起点の記録が無い 10 区間を両端に置いても e_r は変わらない。起点を区間の始まりにした参考の値では 4 種類の e_r が変わる（上の「参考」）。

### 問い全体：holds

少なくとも 1 つの種類で e_r が出て（answer:approve_landing ほか）、「消さない」も根拠つきで 1 種類（stalled）で決まり、disk_space の消し方の推奨（C2）が不明の両端の 2 通りで同じに決まったので、description の基準で holds。次の turn の上限の推奨（変えない）も決まった。

計画への意味：今の「待ちに入ったらすぐ target 全部を消す」形は、指定の起点では approve_landing・worker_question の答えの待ちと、disk_space のときだけ消す他の待ちで、記録の分布に照らして見合っていた（M⁻_r(0, A) が resume_after_limit の 5.48〜landing_slot の 136.56）。ただし上限で止まった後の resume の待ちは、Q2 の値が起点で分かれ、disk_space の記録では消した 4 件のうち 3 件に代償が出ていたので、C2 で disk_space の候補から外す。stalled は見合っていなかった。着地 slot の待ちは着地の検証の作り直しを数えていない。ask の無い区間の e_r は起点の取り方に左右される。

## 後続の実装の task への推奨

再計画の planner が goal 286 の実装の task にするもの：

1. 消す対象は target 全部（A）のまま。incremental だけ（I）は空く量が 3.4 GiB 少なく、延びは小さくならない。
2. 待ちの種類ごとの最小の経過 e_r（指定の起点）：
   - approve_landing・worker_question の答えの待ち：e_r = 0（今のまま）。
   - stalled の答えの待ち：消さない（答えまでが短い）。
   - ask の無い待ち（needs_session・landing_answer・waiting・returning・held）：直前の turn_finished から e_r = 0。待ちに入った時刻から数えるなら、ask の無い landing_answer・waiting と returning は 5 分（参考の値）。実装の task は runtime が読める起点を選び、それに合う方の値を使う。
   - 上限で止まった後の resume の待ち：Q2 は指定の起点で e_r = 0（M⁻ = 5.48）、区間の始まりを起点にした参考の値では「消さない」で、起点で分かれる。一方、disk_space の削除の記録（Q0）では、この待ちで消した 4 件のうち 3 件が 10 分以内に resume が始まったか次の turn の後にまた消されており、下の 3（C2）でこの待ちを disk_space の候補から外す。今の通常の掃除はこの待ちを消さないので、通常の掃除の対象にも足さない（Q2 の値が起点で分かれるため）。
   - review_held・blocked と e2e の待ち：標本が足りるまで今のまま。
   - 着地 slot の待ち：M は e = 0 で見合うが、着地の検証と e2e の作り直し（Spike 3440 の約 2.2 倍）を入れていない。着地の検証の 1 回の延びを Δt に足して比べ直すか、disk_space のときだけ消す今の形を保つ。
3. disk_space の消し方は C2：turn の上限で止まった後の resume の待ちの run を消す候補から外す。task の基準では、保留の解けた件を残したまま除けるのはこの条件だけ。
4. 次の turn の上限は変えない（消した後の turn が上限で止まる割合は消していない turn と差が無い）。
5. task の基準による結論とは別の提案（判断の材料）：保留の解けた件を 2〜3 件失っても除く対象を多く除く（5〜10 件）なら、C1（上の e_r を disk_space にも当てる）が候補になる。
6. 測り方の手当て：`build_outputs_removed` は job の後に記録され、実際の削除の時刻を持たないので、待ちの区間のどこで消したかが記録から決まらない（この Spike の 22 件の「記録が待ちの後」の行と、範囲で残した不明）。消した時刻を payload に持たせれば、次の見直しで e と残りの待ちが決まる。

## 上限と打ち切り

上限は build と測定に合計 3 時間、記録の読み取りに 60 分。打ち切った条件は無い。

| もの | 回数 | 時間 |
|---|---|---|
| 開始の状態 S（setup.sh。base の lib の test の実行を 20 分で止めた分を含む） | 1 | 約 40 分（18:02:33〜18:43:12Z） |
| S の写し取りと確認 | 1 | 約 1 分 |
| K | 4 | T の和 541 秒（戻しと確認を含めて約 10 分） |
| I | 4 | T の和 1900 秒（同じく約 33 分） |
| A | 2 | T の和 886 秒（同じく約 15 分） |
| batch 全体 | 10 | 18:43:12〜19:39:28Z（約 56 分） |
| build と測定の合計 | — | 約 97 分（上限 180 分） |
| 記録の読み取り（queue の events） | — | 約 3 分（上限 60 分。最初の turn だけ） |
| 記録の解析（最初の turn と、review の差し戻しの後の 2 回の作り直し） | — | 約 20 分、約 50 分、約 60 分 |
