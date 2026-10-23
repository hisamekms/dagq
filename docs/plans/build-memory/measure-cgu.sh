#!/bin/sh
# docs/plans/codegen-units-spike.md の測り直し: V0（既定）と V5
# （CARGO_PROFILE_DEV_CODEGEN_UNITS=64）の (i) cargo build --locked --lib と
# (ii) cargo test --locked --no-run を 4 周、交互に流す。1 本ずつの採取と env は
# measure.sh の one をそのまま使い、CSV は cgu-runs.csv・cgu-samples.csv に分けて書く
# （measure.sh の runs.csv・samples.csv は書き換えない）。
#
# Usage: sh docs/plans/build-memory/measure-cgu.sh all
#   cgu-runs.csv・cgu-samples.csv を消してから、下の順で全部を流す。
#   1 周目 V0→V5、2 周目 V5→V0、3 周目 V0→V5、4 周目 V5→V0。
#   周の境界で同じ variant が続くのは 1→2・3→4 の V5 と 2→3 の V0。
#   各周の各 variant で、その variant の CARGO_TARGET_DIR を消して cold から
#   (i)→(ii) を続けて流し（cold を作り直す単位は「周 × variant」）、(ii) の後に消す。
# Usage: sh docs/plans/build-memory/measure-cgu.sh pair <周> <variant>
#   欠けた周の 1 組を流し直す（CSV は消さず、行は末尾に足す）。
set -u

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
tbase="$root/target/build-memory"
BUILD_MEMORY_PREFIX=cgu-
export BUILD_MEMORY_PREFIX

pair() {
  # variant は target の dir の名前になり rm -rf に渡るので、消す前に V0・V5 だけを許す
  case "$2" in V0|V5) ;; *) echo "unknown variant: $2" >&2; exit 2 ;; esac
  rm -rf "${tbase:?}/$2"
  sh "$here/measure.sh" one "$1" "$2" build
  sh "$here/measure.sh" one "$1" "$2" test-build
  rm -rf "${tbase:?}/$2"
}

case "${1:-}" in
  all)
    rm -f "$here/cgu-runs.csv" "$here/cgu-samples.csv"
    pair 1 V0; pair 1 V5
    pair 2 V5; pair 2 V0
    pair 3 V0; pair 3 V5
    pair 4 V5; pair 4 V0
    ;;
  pair) shift; [ $# -eq 2 ] || { sed -n '8,15p' "$0"; exit 2; }; pair "$@" ;;
  *) sed -n '8,15p' "$0"; exit 2 ;;
esac
