#!/bin/sh
# The real-Podman check of docs/plans/container-oom-detection.md: whether an
# OOM kill of a process in a container can be read afterwards as a fact, and
# whether a kill by the container's limit and one by the machine running out
# of memory can be told apart. A person or the inbox runs it, never a worker.
#
# It uses only the spike's machine dagq-spike
# (docs/plans/container-runtime-spike/machine.sh, which mounts WORKDIR and
# keeps the person's default connection) and passes --connection dagq-spike
# to every podman command; it never touches another machine, the default
# connection, or a queue. Start the machine first, then run it from sh:
#
#   sh docs/plans/container-runtime-spike/machine.sh up WORKDIR 2 2048
#   sh docs/plans/container-oom-detection/oom-cases.sh matrix WORKDIR [LIMIT_MIB]
#   sh docs/plans/container-oom-detection/oom-cases.sh parent WORKDIR [LIMIT_MIB]
#   sh docs/plans/container-runtime-spike/machine.sh down
#
# matrix: the cases C5, C1, C2, C4, C3 in that order (C3, the machine-wide
#   OOM that may leave the machine unsteady, last), each with --rm and then
#   without, twice in a row per pair (no alternating: each pair is an
#   observation of presence, not an average). LIMIT_MIB is the container's
#   --memory (default 512); C3 sets --memory above the machine's memory.
# parent: the method M4 only: C5, C1 and C3 with --rm, twice each, every
#   container under a slice of its own held by a sleeping unit, whose
#   memory.events the machine side reads before and after.
#
# The container has the limits of hardened in src/domain/run_container.rs
# (--memory, --cpus, --pids-limit, --read-only, --tmpfs /tmp, --userns
# keep-id, --user, --cap-drop all, no-new-privileges, --network none) and a
# small public image of a fixed version; the dagq image is not built. The
# main process is a sh standing in for `dagq relay verify`: it reads the
# container's /sys/fs/cgroup before and after its child and writes them to
# WORKDIR/oom/<id>/. Memory is used without a language: head -c from
# /dev/zero into tail, which keeps the one endless line in memory.
#
# Output: one row per run on stdout and appended to WORKDIR/oom/table.md, in
# the columns of the document's table; values are "before>after (after -
# before)". The raw files stay under WORKDIR/oom/ for the note.
set -eu

conn=dagq-spike
image=docker.io/library/debian:12.7-slim
mode=${1:-}
case $mode in matrix | parent) ;; *) sed -n '2,38p' "$0" >&2; exit 2 ;; esac
work=$(cd "${2:?WORKDIR}" && pwd)
limit=${3:-512}
out=$work/oom
mkdir -p "$out"
table=$out/table.md

pm() { podman --connection "$conn" "$@"; }
ssh_m() { podman machine ssh "$conn" "$@"; }

podman machine list --format '{{.Name}} {{.Running}}' | sed 's/\* / /' | grep -qx "$conn true" || {
  echo "the machine $conn is not running; start it with machine.sh up WORKDIR CPUS MEMORY_MIB" >&2
  exit 1
}
pm pull -q "$image" > /dev/null
machine_mib=$(ssh_m "awk '/^MemTotal:/ { print int(\$2 / 1024) }' /proc/meminfo" | tr -d '\r')

# What the main process runs in the container. CASE picks the case; BYTES is
# what the hog asks for; FILL_MIB is the page cache C4 makes first.
# C4 runs in steps the control side gates with files in the run's dir: after
# `before` it waits for go0 (the control side has counted conmon's inotify
# fds), makes the page cache, writes `mid` and mid_done, and waits for go1
# (the control side has counted them again). A step that fails, a signal
# that does not come, max that did not grow, or oom or oom_kill that grew
# before the hog, makes the trial invalid (the file `invalid` says why) and
# the hog does not run; so does the control side, by writing `invalid`
# before go1, when it did not see conmon's watch before and gone after.
inner='
o=/out/$RUN_ID
cg=/sys/fs/cgroup
waitfor() { i=0; while [ ! -e $o/$1 ] && [ $i -lt 120 ]; do sleep 0.5; i=$((i + 1)); done; [ -e $o/$1 ]; }
val() { sed -n "1,/^local/s/^$2 //p" $o/$1; }
invalid() { [ -e $o/invalid ] || echo "$1" > $o/invalid; }
snap() {
  { echo events; cat $cg/memory.events; echo local; cat $cg/memory.events.local
    echo "limit $(cat $cg/memory.max)"; } > $o/$1 2>&1
}
cat /proc/self/cgroup > $o/cgroup 2>&1
snap before
hog() { sh -c "head -c $BYTES /dev/zero | tail > /dev/null; echo \$? > $o/child_exit; exit 101"; }
case $CASE in
  C5) sh -c "echo 101 > $o/child_exit; exit 101" ;;
  C1|C3) hog ;;
  C2) exec tail /dev/zero ;;
  C4)
    waitfor go0 || invalid "no go0 from the control side"
    [ -e $o/invalid ] || dd if=/dev/zero of=$o/fill bs=1M count=$FILL_MIB 2> $o/dd.err || invalid "dd failed"
    rm -f $o/fill || invalid "rm of the fill failed"
    snap mid
    [ "$(val mid max)" -gt "$(val before max)" ] 2> /dev/null || invalid "max did not grow"
    [ "$(val mid oom)" = "$(val before oom)" ] || invalid "oom grew before the hog"
    [ "$(val mid oom_kill)" = "$(val before oom_kill)" ] || invalid "oom_kill grew before the hog"
    : > $o/mid_done
    waitfor go1 || invalid "no go1 from the control side"
    if [ -e $o/invalid ]; then true; else hog; fi
    ;;
esac
code=$?
snap after
echo $code > $o/main_exit
exit $code
'

# The value of KEY in the section SECTION of a snapshot, or -.
field() {
  [ -f "$1" ] || { echo -; return; }
  awk -v s="$2" -v k="$3" '$1 == "events" || $1 == "local" { cur = $1; next }
    cur == s && $1 == k { print $2; found = 1 } END { if (!found) print "-" }' "$1"
}
pair() {
  b=$1 a=$2
  case $b$a in *-*) echo "$b>$a" ;; *) echo "$b>$a ($((a - b)))" ;; esac
}
# The inotify fds conmon (pid $1) holds: its OOM watch, the only one it
# opens, is closed when a notice shows no OOM.
# - when there is no such process (no pid, 0, or gone), so that a conmon
# that has ended is never read as a dropped watch.
inotify_count() {
  case $1 in '' | *[!0-9]* | 0) echo -; return ;; esac
  ssh_m "if [ -d /proc/$1 ]; then sudo ls -l /proc/$1/fd 2> /dev/null | grep -c inotify; else echo -; fi" | tr -d '\r'
}
# Wait up to 120 s for the file $1; fails when it did not come.
wait_file() {
  i=0
  while [ ! -e "$1" ] && [ $i -lt 240 ]; do sleep 0.5; i=$((i + 1)); done
  [ -e "$1" ]
}
# Mark the C4 trial in $dir invalid for $1, keeping the first reason.
c4_invalid() { [ -e "$dir/invalid" ] || echo "$1" > "$dir/invalid"; }
dmesg_counts() {
  ssh_m 'sudo dmesg' | tr -d '\r' | awk '/Memory cgroup out of memory: Killed process/ { m++ }
    /\] Out of memory: Killed process/ { g++ } END { printf "%d %d\n", m, g }'
}

run_case() { # CASE RM ROUND [SLICE]
  case_=$1 rm=$2 round=$3 slice=${4:-}
  id=$(echo "$case_-rm$rm-$round${slice:+-parent}" | tr 'A-Z' 'a-z')
  name=dagq-oom-$id
  dir=$out/$id
  rm -rf "$dir"
  mkdir -p "$dir"
  mem=$limit
  bytes=$((limit * 4 * 1048576))
  [ "$case_" = C3 ] && mem=$((machine_mib * 2)) && bytes=$((machine_mib * 4 * 1048576))
  set -- run --name "$name" \
    --userns keep-id:uid=10001,gid=10001 --user 10001:10001 \
    --read-only --tmpfs /tmp:rw,exec,nosuid,nodev \
    --cap-drop all --security-opt no-new-privileges \
    --memory "${mem}m" --cpus 1 --pids-limit 2048 --network none \
    --volume "$out:/out" \
    --env RUN_ID="$id" --env CASE="$case_" --env BYTES="$bytes" --env FILL_MIB=$((limit * 2))
  [ "$rm" = yes ] && set -- "$@" --rm
  [ -n "$slice" ] && set -- "$@" --cgroup-parent "$slice"
  set -- "$@" "$image" sh -c "$inner"

  set +e
  parent_before=-
  if [ -n "$slice" ]; then
    ssh_m "systemd-run --user --quiet --unit=hold-$name --slice=$slice sleep infinity"
    cgpath=$(ssh_m "systemctl --user show -p ControlGroup --value $slice" | tr -d '\r')
    parent_before=$(ssh_m "cat /sys/fs/cgroup$cgpath/memory.events" | tr -d '\r' | awk '$1 == "oom" || $1 == "oom_kill" { printf "%s ", $2 }')
  fi
  dm_before=$(dmesg_counts)
  # The machine's clock, which stamps the events; the host's may differ.
  since=$(ssh_m 'date -u +%Y-%m-%dT%H:%M:%SZ' | tr -d '\r')
  pre=-
  if [ "$case_" = C4 ]; then
    pm "$@" > "$dir/podman.out" 2>&1 &
    pid=$!
    wait_file "$dir/before" || c4_invalid "the control side did not see before"
    conmon=$(pm inspect --format '{{.State.ConmonPid}}' "$name" 2> /dev/null)
    ino_before=$(inotify_count "$conmon")
    : > "$dir/go0"
    ino_mid=-
    if wait_file "$dir/mid_done"; then
      sleep 2
      ino_mid=$(inotify_count "$conmon")
    else
      c4_invalid "the control side did not see mid_done"
    fi
    case $ino_before in '' | - | 0) c4_invalid "no watch of conmon before ($ino_before)" ;; esac
    [ "$ino_mid" = 0 ] || c4_invalid "the watch of conmon not gone after mid ($ino_mid)"
    : > "$dir/go1"
    wait "$pid"
    podman_exit=$?
    if [ -f "$dir/invalid" ]; then
      pre="無効: $(cat "$dir/invalid")（inotify $ino_before>$ino_mid）"
    else
      pre="有効（inotify $ino_before>$ino_mid）"
    fi
  else
    pm "$@" > "$dir/podman.out" 2>&1
    podman_exit=$?
  fi
  sleep 2
  died=$(pm events --stream=false --since "$since" --filter container="$name" --filter event=died \
    --format '{{.OOMKilled}}/{{.ContainerExitCode}}' 2> /dev/null | tr '\n' ' ')
  if [ "$rm" = no ]; then
    inspect=$(pm inspect --format '{{.State.OOMKilled}}/{{.State.ExitCode}}' "$name" 2>&1)
    pm rm -f "$name" > /dev/null 2>&1
  else
    inspect=-
  fi
  dm_after=$(dmesg_counts)
  parent_after=-
  if [ -n "$slice" ]; then
    parent_after=$(ssh_m "cat /sys/fs/cgroup$cgpath/memory.events" | tr -d '\r' | awk '$1 == "oom" || $1 == "oom_kill" { printf "%s ", $2 }')
    ssh_m "systemctl --user stop hold-$name" > /dev/null 2>&1
  fi
  set -e
  mb=$(echo "$dm_before" | cut -d' ' -f1) ma=$(echo "$dm_after" | cut -d' ' -f1)
  gb=$(echo "$dm_before" | cut -d' ' -f2) ga=$(echo "$dm_after" | cut -d' ' -f2)
  dmesg_rows="memcg+$((ma - mb)) global+$((ga - gb))"
  b=$dir/before a=$dir/after
  row="| $case_ | $rm | $round"
  row="$row | $(pair "$(field "$b" events oom)" "$(field "$a" events oom)")"
  row="$row | $(pair "$(field "$b" events oom_kill)" "$(field "$a" events oom_kill)")"
  row="$row | $(pair "$(field "$b" local oom)" "$(field "$a" local oom)")"
  row="$row | $(pair "$(field "$b" local oom_kill)" "$(field "$a" local oom_kill)")"
  row="$row | $(pair "$(field "$b" events max)" "$(field "$dir/mid" events max)")"
  row="$row | $(awk '$1 == "limit" { print $2 }' "$b" 2> /dev/null || echo -)"
  row="$row | $(cat "$dir/main_exit" 2> /dev/null || echo -) | $(cat "$dir/child_exit" 2> /dev/null || echo -)"
  row="$row | $podman_exit | $inspect | ${died:--} | $dmesg_rows"
  row="$row | $(tr '\n' ' ' < "$dir/cgroup" 2> /dev/null || echo -)"
  # OOMKilled says that oom or oom_kill changed; only oom_kill is a kill.
  kills=$(field "$a" events oom_kill) kills_before=$(field "$b" events oom_kill)
  case "$inspect $died" in *true*) said=true ;; *) said=false ;; esac
  case $kills$kills_before in
    *-*) killed=unknown ;;
    *) [ "$kills" = "$kills_before" ] && killed=no || killed=yes ;;
  esac
  case $said/$killed in
    true/unknown) gap="OOMKilled、中の oom_kill は読めない" ;;
    true/no) gap="OOMKilled だが中の oom_kill は増えていない" ;;
    false/yes) gap="中の oom_kill は増えたが OOMKilled が無い" ;;
    true/yes | false/no) gap=なし ;;
    *) gap=- ;;
  esac
  row="$row | $pre | $gap"
  [ -n "$slice" ] && row="$row | ${parent_before:--}> ${parent_after:--}"
  echo "$row |" | tee -a "$table"
}

header="| 場合 | --rm | 回 | oom（中） | oom_kill（中） | local oom | local oom_kill | max（前>C4の途中） | memory.max | 主の exit | 子の exit | podman run の exit | State.OOMKilled/ExitCode | died の OOMKilled/exit | dmesg の OOM の行 | /proc/self/cgroup | C4 の前提（conmon の inotify 前>mid・無効） | OOMKilled と oom_kill の食い違い"
{
  echo
  echo "<!-- $mode $(date -u +%Y-%m-%dT%H:%M:%SZ) limit=${limit}MiB machine=${machine_mib}MiB $(pm version --format 'podman {{.Server.Version}}') $(pm info --format 'runtime {{.Host.OCIRuntime.Name}} {{.Host.OCIRuntime.Version}} cgroup {{.Host.CgroupsVersion}} manager {{.Host.CgroupManager}}' | tr '\n' ' ') kernel $(ssh_m 'uname -r' | tr -d '\r') yama $(ssh_m 'cat /proc/sys/kernel/yama/ptrace_scope 2> /dev/null' | tr -d '\r') -->"
} >> "$table"
case $mode in
  matrix)
    echo "$header |" | tee -a "$table"
    echo "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|" | tee -a "$table"
    for case_ in C5 C1 C2 C4 C3; do
      for rm in yes no; do
        for round in 1 2; do run_case "$case_" "$rm" "$round"; done
      done
    done
    ;;
  parent)
    echo "$header | 親の slice の oom oom_kill（前> 後） |" | tee -a "$table"
    echo "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|" | tee -a "$table"
    for case_ in C5 C1 C3; do
      for round in 1 2; do
        run_case "$case_" yes "$round" "dagq_oom_$(echo "$case_" | tr 'A-Z' 'a-z')_$round.slice"
      done
    done
    ;;
esac
echo "table: $table" >&2
