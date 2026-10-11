#!/bin/sh
# Spike (c)・(d): the Podman machine the spike uses, named dagq-spike, made
# with only WORKDIR mounted (not $HOME), and its removal
# (docs/plans/container-runtime-spike.md). It never starts, stops, removes or
# changes another machine, and it keeps the person's default connection:
# - it refuses to run unless a default connection exists and is not the
#   spike's own, since Podman makes the first connection the default on
#   init whatever --update-connection says;
# - init and start run with --update-connection=false (it refuses to run on a
#   Podman whose `machine init` and `machine start` lack the flag);
# - after init, start and rm (rm picks a new default when it removes the
#   default) it puts the default back if it moved.
#
#   sh machine.sh up WORKDIR CPUS MEMORY_MIB   # init and start
#   sh machine.sh set CPUS MEMORY_MIB          # stop, resize, start
#   sh machine.sh down                         # stop and remove
set -eu
name=dagq-spike

takes_update_connection() {
  podman machine "$1" --help 2>&1 | grep -q -- '--update-connection'
}
default_connection() {
  podman system connection list --format '{{.Name}} {{.Default}}' | awk '$2 == "true" { print $1 }'
}
# The person's default connection, or stop: none, or the spike's own.
persons_default() {
  current=$(default_connection)
  case $current in
    '')
      echo "there is no default connection; Podman would make $name the default. A person sets one first (podman system connection default NAME)." >&2
      exit 1
      ;;
    "$name" | "$name"-*)
      echo "the default connection is $current, the spike's own; a person sets their own default back first." >&2
      exit 1
      ;;
  esac
  echo "$current"
}
keep_default() {
  after=$(default_connection)
  [ "$1" != "$after" ] || return 0
  if podman system connection list --format '{{.Name}}' | grep -qx "$1"; then
    podman system connection default "$1"
    echo "the default connection moved to ${after:-none}; put it back to $1" >&2
  else
    echo "the default connection $1 is gone (now ${after:-none}); a person decides" >&2
    exit 1
  fi
}
others_running() {
  podman machine list --format '{{.Name}} {{.Running}}' |
    awk -v me="$name" '{ sub(/\*$/, "", $1) } $1 != me && $2 == "true" { print $1 }'
}
exists() {
  podman machine list --format '{{.Name}}' | sed 's/\*$//' | grep -qx "$name"
}
start() {
  running=$(others_running)
  if [ -n "$running" ]; then
    echo "another machine is running ($running); a person decides whether to stop it. Not starting $name." >&2
    exit 1
  fi
  podman machine start --update-connection=false "$name"
  keep_default "$before"
}
for sub in init start; do
  takes_update_connection "$sub" || {
    echo "podman machine $sub has no --update-connection; this Podman may move the default connection. Not running." >&2
    exit 1
  }
done

case ${1:-} in
  up)
    work=$(cd "${2:?WORKDIR}" && pwd)
    cpus=${3:?CPUS}
    memory=${4:?MEMORY_MIB}
    if exists; then
      echo "$name exists; run 'sh machine.sh down' first" >&2
      exit 1
    fi
    case $HOME/ in "$work"/*) echo "$work holds \$HOME; give a dir under it or elsewhere" >&2; exit 1 ;; esac
    before=$(persons_default)
    podman machine init --update-connection=false --cpus "$cpus" --memory "$memory" --disk-size 60 \
      --volume "$work:$work" "$name"
    keep_default "$before"
    podman machine inspect --format '{{range .Mounts}}{{.Source}} {{end}}' "$name" |
      tr ' ' '\n' | sed '/^$/d' | while read -r source; do
        [ "$source" = "$work" ] || { echo "unexpected mount $source; run down" >&2; exit 1; }
      done
    start
    ;;
  set)
    cpus=${2:?CPUS}
    memory=${3:?MEMORY_MIB}
    before=$(persons_default)
    podman machine stop "$name"
    podman machine set --cpus "$cpus" --memory "$memory" "$name"
    start
    ;;
  down)
    before=$(persons_default)
    if exists; then
      podman machine stop "$name" || true
      podman machine rm -f "$name"
    fi
    keep_default "$before"
    echo "default connection: $before -> $(default_connection)" >&2
    ;;
  *)
    sed -n '2,16p' "$0" >&2
    exit 2
    ;;
esac
