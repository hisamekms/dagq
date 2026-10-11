#!/bin/sh
# Spike (d): N containers of the dagq-spike machine, each with its own
# sccache server on its own loopback (--network none), building this
# repository at once with the same SCCACHE_DIR mounted
# (docs/plans/container-runtime-spike.md). Needs `container-build.sh
# prepare` first. Each round starts from fresh targets; the cache dir is kept
# between rounds unless the round is named cold.
#
#   sh sccache-concurrency.sh WORKDIR N ROUND [CACHE_SIZE] [JOBS]
#       ROUND cold empties the cache dir first; CACHE_SIZE is SCCACHE_CACHE_SIZE
#       (default 10G); JOBS is each container's CARGO_BUILD_JOBS (default 2)
# prints CSV rows (secs is the container's own build time):
#   n,round,container,secs,exit,requests,hits,misses,cache_errors,read_errors,write_errors,cache_kib
set -eu
conn=dagq-spike
pod() { podman --connection "$conn" "$@"; }

work=$(cd "${1:?WORKDIR}" && pwd)
n=${2:?N}
round=${3:?ROUND}
size=${4:-10G}
jobs=${5:-2}
src=$work/src
cache=$work/sccache
channel=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$src/rust-toolchain.toml")
image=docker.io/library/rust:$channel
[ "$round" != cold ] || rm -rf "$cache"
mkdir -p "$cache" "$work/logs"

i=1
while [ "$i" -le "$n" ]; do
  target=$work/target-sccache-$i
  rm -rf "$target"
  out=$work/logs/sccache-$n-$round-$i
  # The server is started before the build, as the supervisor will
  # (ADR-t2114-4 decision 1); its error log is a file under WORKDIR.
  pod run -d --name "spike-sccache-$i" --network none \
    --read-only --tmpfs /tmp:rw,exec,nosuid,nodev --cap-drop all --security-opt no-new-privileges --pids-limit 4096 \
    -v "$work:$work" -w "$src" \
    -e CARGO_HOME="$work/cargo-home" -e RUSTUP_HOME="$work/rustup-home" \
    -e CARGO_TARGET_DIR="$target" -e CARGO_BUILD_JOBS="$jobs" -e CARGO_TERM_COLOR=never \
    -e SCCACHE_DIR="$cache" -e SCCACHE_CACHE_SIZE="$size" -e SCCACHE_IDLE_TIMEOUT=0 \
    "$image" sh -c "
      SCCACHE_ERROR_LOG=$out.server.log SCCACHE_LOG=warn $work/tools/bin/sccache --start-server &&
      s=\$(date +%s); RUSTC_WRAPPER=$work/tools/bin/sccache cargo build --locked --offline; code=\$?;
      echo \$((\$(date +%s) - s)) > $out.secs;
      $work/tools/bin/sccache --show-stats --stats-format json > $out.stats.json;
      $work/tools/bin/sccache --stop-server > /dev/null; exit \$code" > /dev/null
  i=$((i + 1))
done
i=1
while [ "$i" -le "$n" ]; do
  code=$(pod wait "spike-sccache-$i")
  out=$work/logs/sccache-$n-$round-$i
  pod logs "spike-sccache-$i" > "$out.build.log" 2>&1
  pod rm "spike-sccache-$i" > /dev/null
  stats=$(python3 -I -c '
import json, sys
s = json.load(open(sys.argv[1])).get("stats", {})
def total(v): return sum(v.get("counts", {}).values()) if isinstance(v, dict) else (v or 0)
print(",".join(str(x) for x in (
    s.get("compile_requests", 0), total(s.get("cache_hits")), total(s.get("cache_misses")),
    total(s.get("cache_errors")),
    s.get("cache_read_errors", 0), s.get("cache_write_errors", 0))))
' "$out.stats.json" 2> /dev/null || echo ",,,,,")
  echo "$n,$round,$i,$(cat "$out.secs" 2> /dev/null),$code,$stats,$(du -sk "$cache" | cut -f1)"
  i=$((i + 1))
done
