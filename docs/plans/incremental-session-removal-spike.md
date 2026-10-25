---
id: plan-incremental-session-removal-spike
type: plan
title: スパイク：build の動いていない run の incremental の crate の dir から最新の完了済み session 以外を消したときの、次の build の時間と空く量（編集の後と rebase の後）
status: completed
created: 2026-10-26
owners:
  - hisamekms
tags:
  - planning
  - performance
  - measurement
related:
  - plan-run-build-output-size-spike
  - design-supervisor-lifecycle-run-worktrees
  - development-local-checks
---

# スパイク：incremental の古い session を消したときの次の build

問いは、build の動いていない run の worktree の target で、rustc の incremental の crate ごとの dir（`target/debug/incremental/<crate>-<hash>/`）から、最新の完了済み session とその lock を残してそれより古い完了済み session とその lock を消したとき、(i) 次の build が失敗も full rebuild もせず、(ii) 次の build の時間が消さないときより延びず、(iii) どれだけ空くか。
[run の target の内訳の Spike](run-build-output-size-spike.md) の推奨 1 の前提を測るもの。

判定は、シナリオ E（worker の編集の後の build）が holds、シナリオ R（rebase の後の着地の検証の build）が holds で、問い全体は **holds**（下の「判定と根拠」）。

要点：

- 消した回（D）の build は 20 回とも成功し、依存の crate の compile し直しは無く、時間の比は E で r = 0.85・r_pair = 1.01、R で r = 1.08・r_pair = 1.08（どちらも 1.10 以下）。
- 消すと incremental の 5.29 GiB のうち 2.28 GiB（43%）が空いた（E・R とも同じ値）。
- rustc は build の終わりに、自分の session を除いた完了済み session のうち最新の 1 つを残して古いものを消す。そのため build の後には crate の dir ごとに session が 2 つ（今の build のものと 1 つ前のもの）残る。消さない回（K）も消した回（D）も、build の後の incremental は同じ大きさ（E 5.17〜5.21 GiB、R 5.72〜5.75 GiB）に戻った。消して空くのは、次の build までの間だけになる。

## 環境

| 項目 | 値 |
|---|---|
| 対象の commit | P = `6aabe6c2a`（シナリオ R の写し取りの元）、Q = `2441d29b7`（シナリオ E の元と、R の build の tree。差分は `0ebc13d14` の src 10 file・tests 2 file）。この run の base と branch の起点は `670cd2ff4` |
| rustc / cargo | 1.98.1（48a229cea 2026-09-01）/ 1.98.1（797e8a9bc 2026-08-05） |
| sccache | 0.18.0（run の dagq-rustc-wrapper 経由。server は host の全 run が共有） |
| host | Apple M1・8 コア、macOS 14.7.4（Darwin 23.6.0）。APFS の空きは測定の間に 18〜39 GiB |
| env | この run の worker の `[run.env]` のまま（`CARGO_BUILD_JOBS=4`・`RUST_TEST_THREADS=6`・`NEXTEST_TEST_THREADS=6`・sccache）。TMPDIR は run の一時ファイルの dir |
| provider | Claude の非対話の worker（この run）。打ったものは全て worker の turn の中、自分の worktree の target |
| 同時の load | 本番の他の run が同時に build していた。測定の 20 回の始める前の 1 分値は 4.6〜13.1（16 を超えた回は無い） |

## 打ったもの

script と生の CSV・cargo の出力は run の TMPDIR に置き、commit していない。

### 安全性の根拠（rustc 1.98.1 の `compiler/rustc_incremental/src/persist/fs.rs`）

commit `48a229ceaefd4985c50990b14116b6d856af0985` の file を読んだ。

- session の dir の名前：`generate_session_dir_path` は作業中の dir を `s-{timestamp}-{random}-working` で作る。timestamp は UNIX epoch からのマイクロ秒を固定長の 36 進で書き、先頭の 3 文字（今は `000`）を落としたもの、random は u32 の乱数を固定長の 36 進で書いたもの。`finalize_session_directory` は成功した build の終わりに末尾の `working` を crate の SVH（固定長の 36 進）に替えて改名する（`s-{timestamp}-{random}-{SVH}`）。module の doc comment は改名先を `s-{timestamp}-{SVH}` と書くが、コードは random を残す。読んだ名前は planner の確かめた規則（`s-<T>-<R>-<SVH>`）と同じだった。compile が誤りで終わったときは作業中の dir と lock を消し、改名しない。
- lock の名前：`lock_file_path` は dir の名前のハイフンがちょうど 3 つであることを求め、3 つめのハイフンより前に `.lock` を付ける（`s-<T>-<R>.lock`）。作業中の dir と、それを改名した完了済みの dir は同じ lock を使う。
- どの session を読むか：`find_source_directory_in_iter` は `-working` でない session の dir のうち、名前の timestamp が最も新しい 1 つだけを選び、その中身を新しい作業中の dir に hard link で写す（doc comment の手順 2・3）。写し取りに失敗したとき（共有の lock が取れないときなど）だけ、次に新しい完了済み session に移る。build の動いていない target では最新の写し取りは失敗しないので、古い完了済み session は読まれない。
- 古い session をいつ消すか：`finalize_session_directory` は最後に `garbage_collect_session_directories` を呼ぶ。そこでは lock の無い session の dir と、作られて 10 秒を過ぎた dir の無い lock と、10 秒を過ぎて lock の取れる `-working` の dir を消し、完了済みの session は「今の session の dir を除いて」（コメントは #147821、同じ process の lock を見分けられないため）排他の lock が取れたものを候補にし、`all_except_most_recent` で候補のうち最新の 1 つを残して消す。今の session を候補から除くので、build の後には今の session と 1 つ前の session の 2 つが残る。1 つ前の session は次の build の終わりに rustc が消す。
- 同時の読み手：他の process が写し取りの元にする完了済み session には共有の lock が掛かり、rustc の GC は排他の lock が取れない session を消さない。del.sh は lock を取らないので、build が動いていない run の target でだけ流す（goal 224 の制約と同じ）。

### del.sh

```sh
#!/bin/sh
# del.sh <target/debug/incremental>
# crate の dir ごとに、-working で終わらない session の dir を名前の順（= 時刻の順）に並べ、
# 最新の 1 つを残してそれより古い dir と、その lock（s-<T>-<R>.lock）を消す。
# -working の dir と、その lock には触れない。出力は「消す前の KiB 消した KiB」。
set -eu
inc=$1
before=$(du -sk "$inc" | awk '{print $1}')
for crate in "$inc"/*/; do
  crate=${crate%/}
  old=$(find "$crate" -mindepth 1 -maxdepth 1 -type d -name 's-*' ! -name '*-working' \
          -exec basename {} \; | sort | sed '$d')
  for d in $old; do
    rm -rf "$crate/$d"
    rm -f "$crate/${d%-*}.lock"
  done
done
after=$(du -sk "$inc" | awk '{print $1}')
echo "$before $((before - after))"
```

### 1 回を流す script（run.sh）

`batch.sh` は `run.sh` を回と対と条件の並びで順に呼ぶだけ。

```sh
#!/bin/sh
# run.sh SCEN(E|R) IDX PAIR COND(K|D) : 1 回を流して CSV に 1 行足す
set -u
SCEN=$1 IDX=$2 PAIR=$3 COND=$4
W=$(cat "$TMPDIR/sp/worktree")
SP=$TMPDIR/sp
P=6aabe6c2a Q=2441d29b7
cd "$W"
LOG=$SP/log-$SCEN-$IDX
rm -rf target && python3 -I "$SP/clone.py" "$SP/snap-$SCEN" target
git checkout -q -- .
if [ "$SCEN" = E ]; then
  git checkout -q "$Q" -- src/domain/disk.rs
  git apply "$SP/E1.patch" && git apply "$SP/E2.patch"
else
  git switch -q --detach "$P" && git switch -q --detach "$Q"
fi
inc=target/debug/incremental
load=$(sysctl -n vm.loadavg | awk '{print $2}')
if [ "$COND" = D ]; then
  set -- $(sh "$SP/del.sh" "$inc")
  before_kb=$1 removed_kb=$2
else
  before_kb=$(du -sk "$inc" | awk '{print $1}') removed_kb=0
fi
stats() { sccache --show-stats --stats-format json | jq -r '"\(.stats.cache_hits.counts.Rust // 0) \(.stats.cache_misses.counts.Rust // 0)"'; }
set -- $(stats); h0=$1 m0=$2
s=$(date +%s); timeout 900 cargo test --locked --lib --no-run >"$LOG-lib" 2>&1; rc_lib=$?; lib=$(( $(date +%s) - s ))
s=$(date +%s); timeout 900 cargo test --locked --test it --no-run >"$LOG-it" 2>&1; rc_it=$?; it=$(( $(date +%s) - s ))
set -- $(stats); dh=$(( $1 - h0 )) dm=$(( $2 - m0 ))
after_kb=$(du -sk "$inc" | awk '{print $1}')
comp=$(cat "$LOG-lib" "$LOG-it" | awk '/^ *Compiling /{print $2}' | sort | uniq -c | awk '{printf "%s:%s ", $2, $1}')
g() { echo "$1" | awk '{printf "%.2f", $1/1048576}'; }
echo "$SCEN,$IDX,$PAIR,$COND,$lib,$it,$((lib+it)),$load,$(g $before_kb),$(g $removed_kb),$(g $after_kb),$dh/$dm,$rc_lib/$rc_it,$comp" | tee -a "$SP/results.csv"
```

開始の状態の写し取りと戻しは、description の `cp -cRp` でなく、hard link を保つ APFS の clone（`clone.py`）にした。
`cp -cRp` は file ごとに clone するので、新しい session と 1 つ前の session が hard link で共有する file を別の file に分けてしまう。
最初に `cp -cRp` で写した E の 3 回は、消す前の incremental を 6.11 GiB・消した量を 3.11 GiB と数え、本物の target の値（5.29 GiB・2.28 GiB）より多く出た。
これを道具の誤りとして捨て、E の写し取りを build からやり直して 10 回を測り直した（捨てた 3 回の時間は 57・56・95 秒で、測り直しの値と同じ範囲）。

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

### patch E1・E2

E1（シナリオ R の写し取りの元では P に当てた。P と Q の `src/domain/disk.rs` の差は module の doc comment だけで、E1 はどちらにも当たる）：

```diff
--- a/src/domain/disk.rs
+++ b/src/domain/disk.rs
@@ -193,6 +193,7 @@ pub struct DiskNeeds {
 /// its `reason`, of an unfinished run too. `None` without either; events
 /// of other kinds or without `bytes` are left out.
 pub fn run_size(events: &[RunEvent]) -> Option<u64> {
+    let _probe = "e1";
     let mut largest: [Option<u64>; RUN_SIZE_EVENTS.len()] = [None; RUN_SIZE_EVENTS.len()];
     for event in events {
         let Some(kind) = RUN_SIZE_EVENTS.iter().position(|kind| *kind == event.kind) else {
```

E2：

```diff
--- a/src/domain/disk.rs
+++ b/src/domain/disk.rs
@@ -215,6 +215,7 @@ pub fn run_size(events: &[RunEvent]) -> Option<u64> {
 
 /// `bytes` in GiB with one decimal, for messages.
 pub fn gib(bytes: f64) -> String {
+    let _probe = "e2";
     format!("{:.1} GiB", bytes / f64::from(1u32 << 30))
 }
 
```

### 開始の状態の作り方

- S_E：Q に detach し、空の target から `cargo test --locked --lib --no-run`（299 秒）と `cargo test --locked --test it --no-run`（228 秒）、E1 を当てて同じ 2 本（50 秒・59 秒）。
- S_R：P に detach し、空の target から同じ 2 本（247 秒・129 秒）、E1 を当てて同じ 2 本（26 秒・26 秒）。最初の試みは `--test it` を 1 つの引数で渡して it の build が 0 秒で失敗したので、空の target からやり直した。
- どちらも、build し直した crate の dir（dagq の 3 dir と tests/it）に完了済みの session が 2 つずつ、build script の dir に 1 つ、`-working` は無かった。incremental は `du -sk` で 5.37 GiB（S_E）・5.35 GiB（S_R）、写し取った後の値は 5.29 GiB。
- S_R の session ごとの `du -sk`（KiB、古い → 新しい）：dagq の 3 dir が 36896 → 36260、1222288 → 1178228、1547628 → 1491040、tests/it が 451812 → 438712。古い session と新しい session は file の数で 86〜98% の inode を共有し（例：dagq の 2 dir で 260 file のうち 239）、共有する file は小さい。消して空くのは共有しない大きな file の分。

## 毎回の行

単位は秒と GiB（`du -sk` を 1024² で割る）。sccache の hit/miss の差は server 全体の数で、同時の他の run の compile も入る（この run が compile したのは incremental の dagq の crate だけで、sccache の対象にならない）。成否は lib と it の終了の status。全ての回で cargo の出力の `Compiling` は `dagq` だけ（lib の build と it の build に 1 行ずつ）で、依存の crate の `Compiling` は無かった。

### シナリオ E（編集の後の build）

| シナリオ | 回 | 対 | 条件 | lib の秒 | it の秒 | 合計秒 T | load（1 分） | 消す前の incremental | 消した GiB | build の後の incremental | sccache の hit/miss の差 | 成否 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| E | 1 | 1 | K | 46 | 57 | 103 | 10.54 | 5.29 | 0 | 5.21 | 0/0 | 0/0 |
| E | 2 | 1 | D | 49 | 59 | 108 | 11.38 | 5.29 | 2.28 | 5.18 | 0/0 | 0/0 |
| E | 3 | 2 | D | 52 | 56 | 108 | 13.06 | 5.29 | 2.28 | 5.19 | 0/0 | 0/0 |
| E | 4 | 2 | K | 50 | 57 | 107 | 11.30 | 5.29 | 0 | 5.19 | 0/0 | 0/0 |
| E | 5 | 3 | K | 65 | 47 | 112 | 8.67 | 5.29 | 0 | 5.19 | 0/0 | 0/0 |
| E | 6 | 3 | D | 30 | 29 | 59 | 12.56 | 5.29 | 2.28 | 5.17 | 0/0 | 0/0 |
| E | 7 | 4 | D | 21 | 20 | 41 | 8.35 | 5.29 | 2.28 | 5.18 | 0/0 | 0/0 |
| E | 8 | 4 | K | 24 | 24 | 48 | 5.66 | 5.29 | 0 | 5.18 | 0/0 | 0/0 |
| E | 9 | 5 | K | 32 | 31 | 63 | 4.63 | 5.29 | 0 | 5.18 | 66/0 | 0/0 |
| E | 10 | 5 | D | 36 | 52 | 88 | 6.17 | 5.29 | 2.28 | 5.20 | 0/0 | 0/0 |

### シナリオ R（rebase の後の build）

| シナリオ | 回 | 対 | 条件 | lib の秒 | it の秒 | 合計秒 T | load（1 分） | 消す前の incremental | 消した GiB | build の後の incremental | sccache の hit/miss の差 | 成否 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| R | 1 | 1 | K | 58 | 41 | 99 | 8.83 | 5.29 | 0 | 5.72 | 0/0 | 0/0 |
| R | 2 | 1 | D | 58 | 49 | 107 | 6.88 | 5.29 | 2.28 | 5.73 | 0/0 | 0/0 |
| R | 3 | 2 | D | 57 | 50 | 107 | 5.62 | 5.29 | 2.28 | 5.73 | 0/0 | 0/0 |
| R | 4 | 2 | K | 49 | 44 | 93 | 7.19 | 5.29 | 0 | 5.72 | 0/0 | 0/0 |
| R | 5 | 3 | K | 53 | 74 | 127 | 5.14 | 5.29 | 0 | 5.74 | 0/0 | 0/0 |
| R | 6 | 3 | D | 88 | 67 | 155 | 10.29 | 5.29 | 2.28 | 5.75 | 1/1 | 0/0 |
| R | 7 | 4 | D | 59 | 43 | 102 | 10.45 | 5.29 | 2.28 | 5.72 | 0/0 | 0/0 |
| R | 8 | 4 | K | 56 | 39 | 95 | 6.22 | 5.29 | 0 | 5.73 | 0/0 | 0/0 |
| R | 9 | 5 | K | 76 | 57 | 133 | 5.36 | 5.29 | 0 | 5.74 | 0/0 | 0/0 |
| R | 10 | 5 | D | 48 | 57 | 105 | 7.82 | 5.29 | 2.28 | 5.74 | 0/0 | 0/0 |

### 集計

| シナリオ | K の T | D の T | M_K | M_D | r | 対ごとの r_j | r_pair | 消した GiB の中央値 | 消す前に対する割合 | 判定 |
|---|---|---|---|---|---|---|---|---|---|---|
| E | 103・107・112・48・63 | 108・108・59・41・88 | 103 | 88 | 0.85 | 1.05・1.01・0.53・0.85・1.40 | 1.01 | 2.28 | 43% | holds |
| R | 99・93・127・95・133 | 107・107・155・102・105 | 99 | 107 | 1.08 | 1.08・1.15・1.22・1.07・0.79 | 1.08 | 2.28 | 43% | holds |

### 最後の D の回の後

| シナリオ | `cargo test --locked --lib domain::disk` | crate の dir ごとの完了済み session の数 | `-working` |
|---|---|---|---|
| E（10 回目の後、tree は Q+E1+E2） | 終了の status 0、4 passed・0 failed | dagq の 3 dir と tests/it が 2 つずつ、build script が 1 つ（lock も同じ数） | どの dir にも無い |
| R（10 回目の後、tree は Q） | 終了の status 0、4 passed・0 failed | dagq の 3 dir と tests/it が 2 つずつ、build script が 1 つ（lock も同じ数） | どの dir にも無い |

D の build の後に 2 つあるのは、消して残した session と今の build の session で、上の rustc の GC の規則どおり。

## 判定と根拠

評価は description の順（1 失敗・full rebuild → 2 打ち切り・道具・高 load → 3 時間の比）に当てた。

### シナリオ E：holds（段 3 で決まった）

- 段 1 に当たらない：D の 5 回の lib と it の build は全て終了の status 0。依存の crate の `Compiling` は無く、D の T の最大 108 秒は K の T の最大の 2 倍（224 秒）を超えない。最後の D の後の `domain::disk` の test は通った。
- 段 2 に当たらない：K と D は 5 回ずつ揃い、K の build は全て成功し、15 分を超えた build は無い。load の 1 分値が 16 を超えた回は 0 回。
- 段 3：r = 88 / 103 = 0.85、r_pair = 1.01 で、どちらも 1.10 以下。

### シナリオ R：holds（段 3 で決まった）

- 段 1 に当たらない：D の 5 回は全て成功し、依存の crate の `Compiling` は無く、D の T の最大 155 秒は K の最大の 2 倍（266 秒）を超えない。最後の D の後の `domain::disk` の test は通った。
- 段 2 に当たらない：K と D は 5 回ずつ揃い、K の build は全て成功し、load が 16 を超えた回は 0 回。
- 段 3：r = 107 / 99 = 1.08、r_pair = 1.08 で、どちらも 1.10 以下。対 2・3 の r_j は 1.15・1.22 だが、対 5 は 0.79 で、同じ条件の T の揺れ（K で 93〜133 秒）の中にある。

### 問い全体：holds

E と R の両方が holds。rustc は新しい session を作るとき最新の完了済み session だけを写し取りの元にするので、それより古い session は読まれず、消しても次の build が使う中身は変わらない。測った時間もそれと合う。

## 後続の計画（draft 3461）への推奨

- 消す対象：build の動いていない run の `target/debug/incremental/<crate>-<hash>/` の、`-working` でない session のうち名前の時刻が最新のもの以外と、その lock（`s-<T>-<R>.lock`）。`-working` の dir とその lock には触れない。lock の名前は dir の名前の 3 つめのハイフンより前に `.lock` を付けたもの（rustc の `lock_file_path`）。
- 消す時期：掃除の job の通常の周回で消してよい。時間の代償は E・R とも測れなかった（r ≤ 1.08）。ただし rustc は build のたびに 1 つ前の session を残すので、次の build の後には同じ大きさに戻る（K と D の build の後の incremental が同じ）。消して空くのは run が次に build するまでの間で、run が build するたびに（差し戻しの後・rebase と着地の検証の後）消し直す形になる。差し戻しの答えや着地の slot を待つ run は build が止まっている時間が長いので、その間の空きとして効く。
- 空く量の見積もり：今回の形（lib と it の build を 2 回流した後）で 1 本あたり 2.28 GiB、incremental の 43%。着地の検証の後の target（[run の target の内訳の Spike](run-build-output-size-spike.md) で incremental 7.3G）では、前の session が 2.4G を占めていた（その Spike の C の値）ので、着地待ちの run 1 本で 2〜2.5 GiB と見込む。中央値は 0.5 GiB を大きく超えるので、効きが小さいという注記は要らない。
- 守ること：build が動いている run の target には触れない（del.sh は rustc の lock を取らないので、build と競ると読み手の写し取りの元を消しうる）。goal 224 の制約（作業中・validating・review・着地の検証・e2e の最中の run に触れない）がそのまま要る。
- 測らなかったこと：着地の検証そのもの（`landing-it.sh` の nextest の build）と clippy の check の incremental の dir は測っていない。session の規則は同じ rustc のものなので、同じく 2 つ残り、古い方を消せると見込む。

## 使った時間と回数

上限は build と測定の合計 3 時間。使ったのは約 75 分（この run の測定の開始から最後の test まで 4206 秒と、写し取りの確かめ）。打ち切ったシナリオは無く、15 分を超えた build は無い。

| もの | 回数 | build の秒 |
|---|---|---|
| E の開始の状態（`cp -cRp` で写した最初のもの、捨てた） | 1 | 441 |
| E の `cp -cRp` の状態での測定（道具の誤りとして捨てた） | 3 | 208 |
| E の開始の状態（hard link を保つ clone） | 1 | 636 |
| E の測定 | 10（K 5・D 5） | 837 |
| R の開始の状態（引数の誤りで it が失敗、捨てた） | 1 | 247 |
| R の開始の状態 | 1 | 428 |
| R の測定 | 10（K 5・D 5） | 1123 |
| 最後の D の後の `domain::disk` の test | 2 | 数秒ずつ |

シナリオごと：E は測定の 10 回と開始の状態で 1473 秒（捨てた分を含めると 2122 秒）、R は 1551 秒（捨てた分を含めると 1798 秒）。
