#!/bin/sh
# Spike (b): the time and disk of a run's own clone of REPO, made by a path
# with --no-hardlinks and by a file:// URL, 1, 2 and 4 at once, and the time
# of a run branch's git bundle and of a fetch from it
# (docs/plans/container-runtime-spike.md). It only reads REPO; everything it
# writes is under WORKDIR, which it removes at the end unless KEEP=1.
#
#   sh clone-cost.sh REPO WORKDIR [CHANGE_KIB]
# prints CSV rows: kind,method,concurrent,secs,clone_kib,git_kib,bytes
set -eu
repo=$(cd "${1:?usage: sh clone-cost.sh REPO WORKDIR [CHANGE_KIB]}" && pwd)
work=${2:?usage: sh clone-cost.sh REPO WORKDIR [CHANGE_KIB]}
change_kib=${3:-256}
[ ! -e "$work" ] || { echo "$work exists; give a new dir" >&2; exit 1; }
mkdir -p "$work"
work=$(cd "$work" && pwd)
[ "${KEEP:-0}" = 1 ] || trap 'rm -rf "$work"' EXIT

now() { python3 -I -c 'import time; print(f"{time.time():.3f}")'; }
secs() { python3 -I -c "print(f'{$2 - $1:.3f}')"; }
kib() { du -sk "$1" | cut -f1; }

echo "# source: $(git -C "$repo" count-objects -v | tr '\n' ' ')" >&2
echo "kind,method,concurrent,secs,clone_kib,git_kib,bytes"
for method in path url; do
  case $method in
    path) src=$repo; opt=--no-hardlinks ;;
    url) src=file://$repo; opt= ;;
  esac
  for n in 1 2 4; do
    rm -rf "$work/c"
    mkdir "$work/c"
    t0=$(now)
    pids=
    i=1
    while [ "$i" -le "$n" ]; do
      git clone -q $opt "$src" "$work/c/$i" &
      pids="$pids $!"
      i=$((i + 1))
    done
    for pid in $pids; do wait "$pid" || { echo "a clone failed" >&2; exit 1; }; done
    t1=$(now)
    echo "clone,$method,$n,$(secs "$t0" "$t1"),$(kib "$work/c/1"),$(kib "$work/c/1/.git"),"
  done
done

# A run branch: one commit with CHANGE_KIB of new text on the clone's HEAD,
# bundled in the run's clone and fetched into another clone (the control
# side here), never into REPO.
rm -rf "$work/c"
git clone -q --no-hardlinks "$repo" "$work/run"
git clone -q --no-hardlinks "$repo" "$work/control"
base=$(git -C "$work/run" rev-parse HEAD)
git -C "$work/run" switch -q -c spike/run
head -c $((change_kib * 768)) /dev/urandom | base64 > "$work/run/spike-change.txt"
git -C "$work/run" add spike-change.txt
git -C "$work/run" -c user.name=spike -c user.email=spike@localhost \
  -c commit.gpgsign=false -c core.hooksPath=/dev/null commit -q -m spike
t0=$(now)
git -C "$work/run" bundle create -q "$work/run.bundle" "$base..spike/run"
t1=$(now)
echo "bundle_create,,1,$(secs "$t0" "$t1"),,,$(wc -c < "$work/run.bundle" | tr -d ' ')"
t0=$(now)
git -C "$work/control" fetch -q --no-tags "$work/run.bundle" spike/run:refs/spike/run
t1=$(now)
echo "bundle_fetch,,1,$(secs "$t0" "$t1"),,,"
