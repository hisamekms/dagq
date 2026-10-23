#!/bin/sh
# docs/plans/build-memory.md の測定: dagq の build と test の process ごとと合計の peak の
# メモリ（RSS）を、host の自分の worktree で 1 本ずつ直列に測る。標準の道具（sh・ps・
# /usr/bin/time・awk・sysctl）だけを使う。
#
# Usage: sh docs/plans/build-memory/measure.sh all
#        sh docs/plans/build-memory/measure.sh one <周> <variant> <対象>
#   all  下の順で全部を流す（runs.csv・samples.csv に追記する。先に消してから流す）。
#        (i)・(ii) は variant ごとに 2 周: 1 周目 V0→V1→…→V6、2 周目 V6→V5→…→V0。
#        周の中では同じ variant を続けて流さず、周の境界の V6 だけが続く。
#        各周の各 variant で、その variant の CARGO_TARGET_DIR を消して cold から
#        (i)→(ii) を続けて流す（cold を作り直す単位は「周 × variant」）。
#        V0 以外の target は (ii) の後に消す（disk のため）。
#        (iii) は T6・T2 を 2 周: 1 周目 T6→T2、2 周目 T2→T6（境界の T2 が続く）。
#        test の binary は (ii) の V0 の 2 周目の target をそのまま使う。
#   one  1 周・1 variant・1 対象を流す。<対象> は build（(i)）・test-build（(ii)）・
#        unit（(iii)）。build の前に target を消すのは all だけが行う。
#
# variant: V0 既定（Cargo.toml の profile.dev と dagq.toml の [run.env]）、
#   V1 CARGO_BUILD_JOBS=1、V2 CARGO_BUILD_JOBS=2、V3 CARGO_INCREMENTAL=0、
#   V4 CARGO_PROFILE_DEV_CODEGEN_UNITS=16、V5 CARGO_PROFILE_DEV_CODEGEN_UNITS=64、
#   V6 CARGO_PROFILE_DEV_DEBUG=0。(iii) は T6（[run.env] の NEXTEST_TEST_THREADS=6）
#   と T2（NEXTEST_TEST_THREADS=2）で、どちらも V0 の target を使う。
# どの run も RUSTC_WRAPPER を空にする（rustc が sccache の server の子になると cargo の
#   process の木に入らないため）。CARGO_BUILD_JOBS などの [run.env] の値は env で与える。
#   Cargo.toml と dagq.toml は変えない。
#
# 採取: 1 秒ごとに ps で /usr/bin/time の下の process の木（time 自身を除く）の各 process の
#   RSS（KiB）を採り、木の合計と最大の process（rustc は --crate-name を添える）を
#   samples.csv に書く。1 秒より短い山は取りこぼしうるので、/usr/bin/time -l の
#   maximum resident set size（byte）を runs.csv に並べる。
# 出力（この script の dir）:
#   samples.csv: round,variant,target,elapsed_s,tree_rss_kib,max_proc_rss_kib,max_proc_name
#   runs.csv:    round,variant,target,start_utc,end_utc,seconds,exit,peak_proc_kib,
#                peak_proc_name,peak_total_kib,time_maxrss_bytes,load1_start,load1_end
#   各 run の cargo の出力と time -l の出力は $TMPDIR/build-memory-logs に置く（commit しない）。
#   BUILD_MEMORY_OUT に dir を渡すと、CSV をこの script の dir でなくそこに書く（試しの run 用）。
#   BUILD_MEMORY_PREFIX に文字列を渡すと、CSV と log の dir の名前の頭に付ける（別の測定の
#   CSV を上の 2 つと分けるため。measure-cgu.sh が cgu- を渡す）。
set -u

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
out=${BUILD_MEMORY_OUT:-$here}
prefix=${BUILD_MEMORY_PREFIX:-}
runs="$out/${prefix}runs.csv"
samples="$out/${prefix}samples.csv"
logs="${TMPDIR:-/tmp}/${prefix}build-memory-logs"
tbase="$root/target/build-memory"

load1() { sysctl -n vm.loadavg | awk '{print $2}'; }

variant_env() {
  # 標準出力に env の引数を出す
  echo "RUSTC_WRAPPER= CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=6 NEXTEST_TEST_THREADS=6"
  case "$1" in
    V0|T6) ;;
    V1) echo "CARGO_BUILD_JOBS=1" ;;
    V2) echo "CARGO_BUILD_JOBS=2" ;;
    V3) echo "CARGO_INCREMENTAL=0" ;;
    V4) echo "CARGO_PROFILE_DEV_CODEGEN_UNITS=16" ;;
    V5) echo "CARGO_PROFILE_DEV_CODEGEN_UNITS=64" ;;
    V6) echo "CARGO_PROFILE_DEV_DEBUG=0" ;;
    T2) echo "NEXTEST_TEST_THREADS=2" ;;
  esac
}

target_dir() {
  case "$1" in T6|T2) echo "$tbase/V0" ;; *) echo "$tbase/$1" ;; esac
}

# $1: time の pid。木の各 process の rss を合計し、最大の process を出す。
sample_tree() {
  ps -axo pid=,ppid=,rss=,args= | awk -v root="$1" '
    { pid=$1; ppid[pid]=$2; rss[pid]=$3
      n=$4; sub(".*/", "", n); name[pid]=n
      for (i=5; i<=NF; i++) if ($i == "--crate-name") { name[pid]=n "(" $(i+1) ")"; break }
      pids[NR]=pid }
    END {
      total=0; max=0; mname=""
      for (k in pids) {
        p=pids[k]; q=p; inside=0
        for (d=0; d<64 && q!="" && q>1; d++) { q=ppid[q]; if (q==root) { inside=1; break } }
        if (!inside) continue
        total+=rss[p]
        if (rss[p]+0 > max) { max=rss[p]+0; mname=name[p] }
      }
      printf "%d,%d,%s\n", total, max, mname
    }'
}

one() {
  round=$1 variant=$2 target=$3
  case "$target" in
    build) cmd="cargo build --locked --lib" ;;
    test-build) cmd="cargo test --locked --no-run" ;;
    unit) cmd="cargo nextest run --locked --lib --bins" ;;
    *) echo "unknown target: $target" >&2; exit 2 ;;
  esac
  mkdir -p "$logs"
  [ -f "$runs" ] || echo "round,variant,target,start_utc,end_utc,seconds,exit,peak_proc_kib,peak_proc_name,peak_total_kib,time_maxrss_bytes,load1_start,load1_end" > "$runs"
  [ -f "$samples" ] || echo "round,variant,target,elapsed_s,tree_rss_kib,max_proc_rss_kib,max_proc_name" > "$samples"
  case "$variant" in V[0-6]|T6|T2) ;; *) echo "unknown variant: $variant" >&2; exit 2 ;; esac
  base="$logs/r$round-$variant-$target"
  tdir=$(target_dir "$variant")
  envs=$(variant_env "$variant" | tr '\n' ' ')
  l_start=$(load1)
  start=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  t0=$(date +%s)
  # shellcheck disable=SC2086
  (cd "$root" && exec env $envs CARGO_TARGET_DIR="$tdir" /usr/bin/time -l -o "$base.time" $cmd) > "$base.log" 2>&1 &
  tpid=$!
  peak_proc=0 peak_name="" peak_total=0 elapsed=0
  while st=$(ps -o stat= -p "$tpid") && [ "${st#Z}" = "$st" ]; do
    line=$(sample_tree "$tpid")
    total=${line%%,*}; rest=${line#*,}; mx=${rest%%,*}; mname=${rest#*,}
    if [ "$total" -gt 0 ]; then
      echo "$round,$variant,$target,$elapsed,$total,$mx,$mname" >> "$samples"
      [ "$total" -gt "$peak_total" ] && peak_total=$total
      [ "$mx" -gt "$peak_proc" ] && { peak_proc=$mx; peak_name=$mname; }
    fi
    sleep 1
    elapsed=$(( $(date +%s) - t0 ))
  done
  wait "$tpid"; code=$?
  t1=$(date +%s)
  end=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  l_end=$(load1)
  maxrss=$(awk '/maximum resident set size/ {print $1}' "$base.time" 2>/dev/null)
  echo "$round,$variant,$target,$start,$end,$((t1 - t0)),$code,$peak_proc,$peak_name,$peak_total,${maxrss:-},$l_start,$l_end" >> "$runs"
  echo "r$round $variant $target: ${code} $((t1 - t0))s proc=$peak_proc($peak_name) total=$peak_total maxrss=${maxrss:-}"
}

cold_pair() {
  round=$1 variant=$2
  rm -rf "$tbase/$variant"
  one "$round" "$variant" build
  one "$round" "$variant" test-build
  [ "$variant" = V0 ] || rm -rf "$tbase/$variant"
}

case "${1:-}" in
  all)
    rm -f "$runs" "$samples"
    for v in V0 V1 V2 V3 V4 V5 V6; do cold_pair 1 "$v"; done
    for v in V6 V5 V4 V3 V2 V1 V0; do cold_pair 2 "$v"; done
    for v in T6 T2; do one 1 "$v" unit; done
    for v in T2 T6; do one 2 "$v" unit; done
    ;;
  one) shift; [ $# -eq 3 ] || { sed -n '6,17p' "$0"; exit 2; }; one "$@" ;;
  *) sed -n '6,17p' "$0"; exit 2 ;;
esac
