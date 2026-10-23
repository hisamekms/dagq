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
# up starts the machine only after it reads that WORKDIR is its one mount, and
# needs jq for that. `podman machine inspect` has no mounts (InspectInfo in
# podman v6.1.2 pkg/machine/config.go; --format applies to it,
# cmd/podman/machine/inspect.go), so up reads the top-level Mounts[].Source
# (the host path left of --volume) of the machine's config file
# <ConfigDir.Path>/<name>.json (pkg/machine/vmconfigs/machine.go and
# config.go). When it cannot read them, it stops before start and leaves the
# machine for down.
#
#   sh machine.sh up WORKDIR CPUS MEMORY_MIB   # init and start
#   sh machine.sh set CPUS MEMORY_MIB          # stop, resize, start
#   sh machine.sh down                         # stop and remove
#
# The spike's named volumes (dagq-spike-*: the build outputs and the sccache
# cache) are on the machine's disk; down removes them while the machine runs
# and then removes the machine with its disk, so none is left.
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
# Stop unless the machine's only mounts have $1 as their source.
only_mount() {
  command -v jq > /dev/null 2>&1 || not_checked "jq is not found"
  if ! dir=$(podman machine inspect --format '{{.ConfigDir.Path}}' "$name"); then
    not_checked "podman machine inspect failed"
  fi
  [ -n "$dir" ] && [ -d "$dir" ] || not_checked "no config dir '$dir'"
  config=$dir/$name.json
  [ -f "$config" ] || not_checked "no config file $config"
  # Prints the sources other than $1 as a JSON array; fails unless the file
  # holds one JSON value whose Mounts is a non-empty array.
  if ! others=$(jq -n -r --arg work "$1" '
      [inputs] | if length == 1 then .[0] else error("not one JSON value") end
      | if (.Mounts | type) == "array" and (.Mounts | length) > 0
        then [.Mounts[] | .Source | select(. != $work)] | tojson
        else error("Mounts is not a non-empty array") end' "$config"); then
    not_checked "cannot read Mounts in $config"
  fi
  if [ "$others" != '[]' ]; then
    echo "unexpected mount(s): $others; not starting $name, run 'sh machine.sh down'" >&2
    exit 1
  fi
}
not_checked() {
  echo "$1; cannot check the mounts of $name, not starting it. Run 'sh machine.sh down'" >&2
  exit 1
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
    only_mount "$work"
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
      if podman machine list --format '{{.Name}} {{.Running}}' | sed 's/\* / /' | grep -qx "$name true"; then
        podman --connection "$name" volume ls --format '{{.Name}}' | grep '^dagq-spike-' |
          while read -r volume; do podman --connection "$name" volume rm -f "$volume" > /dev/null; done || true
      fi
      podman machine stop "$name" || true
      podman machine rm -f "$name"
    fi
    keep_default "$before"
    echo "default connection: $before -> $(default_connection)" >&2
    ;;
  *)
    sed -n '2,28p' "$0" >&2
    exit 2
    ;;
esac
