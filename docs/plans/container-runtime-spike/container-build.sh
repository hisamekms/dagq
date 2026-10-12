#!/bin/sh
# Spike (c)・(d): this repository's cargo build and unit tests in a container
# of the dagq-spike machine (docs/plans/container-runtime-spike.md). Every
# podman command names the machine's connection; the default connection is
# not used. The clone, the toolchain, the crates, the logs and the samples are
# under WORKDIR, the machine's only mount, so the host reads them. The build
# output is not: a target on the virtiofs mount fails to link build scripts
# (cannot find build_script_build-*.rcgu.o), so each LABEL's target is a
# named volume inside the machine, made new before the build (a cold build)
# and removed at the end.
#
#   sh container-build.sh prepare WORKDIR REPO SCCACHE_VERSION
#       clone REPO, pull the image, install the toolchain of
#       rust-toolchain.toml, fetch the crates and install sccache (network)
#   sh container-build.sh run WORKDIR LABEL CPUS
#       a cold build and the unit tests with --network none in the named
#       volume dagq-spike-target-LABEL; prints CSV rows
#       label,step,secs,exit,oom_killed,kernel_oom,peak_mem
set -eu
conn=dagq-spike
pod() { podman --connection "$conn" "$@"; }
now() { python3 -I -c 'import time; print(f"{time.time():.3f}")'; }
secs() { python3 -I -c "print(f'{$2 - $1:.3f}')"; }

cmd=${1:?prepare or run}
work=$(cd "${2:?WORKDIR}" && pwd)
src=$work/src
image() {
  channel=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$src/rust-toolchain.toml")
  echo "docker.io/library/rust:$channel"
}
# The toolchain and the crates live under WORKDIR and the target in a named
# volume, so the root filesystem stays read-only.
env_args="-e CARGO_HOME=$work/cargo-home -e RUSTUP_HOME=$work/rustup-home -e CARGO_TERM_COLOR=never"
hardening="--read-only --tmpfs /tmp:rw,exec,nosuid,nodev --cap-drop all --security-opt no-new-privileges --pids-limit 4096"

case $cmd in
  prepare)
    repo=$(cd "${3:?REPO}" && pwd)
    version=${4:?SCCACHE_VERSION}
    [ -d "$src" ] || git clone -q --no-hardlinks "$repo" "$src"
    mkdir -p "$work/cargo-home" "$work/rustup-home" "$work/tools" "$work/logs"
    pod pull "$(image)"
    # The image's rustup proxies (/usr/local/cargo/bin) look for rustup in
    # CARGO_HOME/bin, so the bin dir is copied into the CARGO_HOME under
    # WORKDIR before rustup runs.
    pod run --rm -v "$work:$work" -w "$src" $env_args "$(image)" sh -c \
      "{ [ -x $work/cargo-home/bin/rustup ] || cp -a /usr/local/cargo/bin $work/cargo-home/; } && rustup toolchain install && cargo fetch --locked && cargo install --locked --no-default-features --root $work/tools sccache --version $version"
    ;;
  run)
    label=${3:?LABEL}
    cpus=${4:?CPUS}
    volume=dagq-spike-target-$label
    target=/spike/target
    trap 'pod rm -f spike-$label-build spike-$label-test > /dev/null 2>&1 || true; pod volume rm -f "$volume" > /dev/null 2>&1 || true' EXIT
    trap 'exit 130' INT TERM
    pod volume rm -f "$volume" > /dev/null 2>&1 || true
    pod volume create "$volume" > /dev/null
    mkdir -p "$work/logs"
    for step in build test; do
      case $step in
        build) run="cargo build --locked --offline --all-targets" ;;
        test) run="cargo test --locked --offline --lib --bins" ;;
      esac
      c=spike-$label-$step
      log=$work/logs/$label-$step.log
      kernel_before=$(podman machine ssh "$conn" "sudo journalctl -k --no-pager | grep -ci 'out of memory' || true" | head -n 1)
      t0=$(now)
      pod run -d --name "$c" --network none $hardening -v "$work:$work" -v "$volume:$target" -w "$src" $env_args \
        -e CARGO_TARGET_DIR="$target" -e CARGO_BUILD_JOBS="$cpus" "$(image)" sh -c "$run" > /dev/null
      (
        while pod container exists "$c" 2> /dev/null; do
          pod stats --no-stream --format '{{.MemUsage}}' "$c" 2> /dev/null || true
          sleep 5
        done
      ) > "$work/logs/$label-$step.mem" &
      sampler=$!
      exit_code=$(pod wait "$c")
      t1=$(now)
      oom=$(pod inspect --format '{{.State.OOMKilled}}' "$c")
      pod logs "$c" > "$log" 2>&1
      pod rm "$c" > /dev/null
      kill "$sampler" 2> /dev/null || true
      wait "$sampler" 2> /dev/null || true
      kernel_after=$(podman machine ssh "$conn" "sudo journalctl -k --no-pager | grep -ci 'out of memory' || true" | head -n 1)
      peak=$(awk '{ print $1 }' "$work/logs/$label-$step.mem" | sed 's/i\{0,1\}B$//' | sort -h | tail -n 1)
      echo "$label,$step,$(secs "$t0" "$t1"),$exit_code,$oom,$((kernel_after - kernel_before)),$peak"
    done
    ;;
  *)
    sed -n '2,18p' "$0" >&2
    exit 2
    ;;
esac
