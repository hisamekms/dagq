#!/bin/sh
# Spike (d): N containers of the dagq-spike machine, each with its own
# sccache server on its own loopback (--network none), building this
# repository at once with the same SCCACHE_DIR mounted
# (docs/plans/container-runtime-spike.md). Needs `container-build.sh
# prepare` first. The cache and the build outputs are named volumes inside
# the machine, not on the WORKDIR mount (a target there fails to link build
# scripts): the cache is the one shared volume dagq-spike-sccache, kept
# between rounds unless the round is named cold, and container i's target is
# dagq-spike-target-sccache-i, made new before each round and removed at the
# end. The logs and the stats are under WORKDIR, so the host reads them.
#
#   sh sccache-concurrency.sh WORKDIR N ROUND [CACHE_SIZE] [JOBS]
#       ROUND cold empties the cache first; CACHE_SIZE is SCCACHE_CACHE_SIZE
#       (default 10G); JOBS is each container's CARGO_BUILD_JOBS (default 2)
# prints CSV rows (secs is the container's own build time; cache_kib is the
# cache volume read inside the machine after that container ends):
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
cache_volume=dagq-spike-sccache
cache=/spike/sccache
target=/spike/target
channel=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$src/rust-toolchain.toml")
image=docker.io/library/rust:$channel
# The cache volume's size, read by du inside the machine at the mountpoint
# the connection's store gives.
cache_kib() {
  podman machine ssh "$conn" "du -sk '$(pod volume inspect --format '{{.Mountpoint}}' "$cache_volume")'" | cut -f1
}
cleanup() {
  i=1
  while [ "$i" -le "$n" ]; do
    pod rm -f "spike-sccache-$i" > /dev/null 2>&1 || true
    pod volume rm -f "dagq-spike-target-sccache-$i" > /dev/null 2>&1 || true
    i=$((i + 1))
  done
}
trap cleanup EXIT
trap 'exit 130' INT TERM

if [ "$round" = cold ]; then
  pod volume rm -f "$cache_volume" > /dev/null 2>&1 || true
fi
pod volume exists "$cache_volume" || pod volume create "$cache_volume" > /dev/null
mkdir -p "$work/logs"

i=1
while [ "$i" -le "$n" ]; do
  volume=dagq-spike-target-sccache-$i
  pod volume rm -f "$volume" > /dev/null 2>&1 || true
  pod volume create "$volume" > /dev/null
  out=$work/logs/sccache-$n-$round-$i
  # The server is started before the build, as the supervisor will
  # (ADR-t2114-4 decision 1); its error log is a file under WORKDIR.
  pod run -d --name "spike-sccache-$i" --network none \
    --read-only --tmpfs /tmp:rw,exec,nosuid,nodev --cap-drop all --security-opt no-new-privileges --pids-limit 4096 \
    -v "$work:$work" -v "$volume:$target" -v "$cache_volume:$cache" -w "$src" \
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
  echo "$n,$round,$i,$(cat "$out.secs" 2> /dev/null),$code,$stats,$(cache_kib)"
  i=$((i + 1))
done
