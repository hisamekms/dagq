#!/bin/sh
# Check that the plugin and the binary carry the same version (ADR-t617-1).
#
# - Always: the `version` of plugins/claude-dagq/.claude-plugin/plugin.json is
#   the `[package].version` of Cargo.toml (decision 3; on main both are
#   X.Y.Z-dev, in a release both are X.Y.Z).
# - When the version is a release (X.Y.Z, no pre-release part): the
#   claude-dagq entry of .claude-plugin/marketplace.json has `ref` vX.Y.Z, the
#   tag the release commit is about to get (decision 4). On a -dev version the
#   entry may keep the ref of the previous release, or have none before the
#   first pinned release (decision 6), so it is not checked.
# - With --tag vX.Y.Z (release.yml): the version is also X.Y.Z of the tag.
#
# Usage: sh scripts/check-plugin-version.sh [--tag vX.Y.Z]
#
# Meant to be run from the repository root; from anywhere else it changes to
# the repository root found from the script's own location.
#
# Exit 0 when the versions agree, 1 when one does not (each mismatch goes to
# stderr with the file and both values), 2 when a file or a version is not
# found or the arguments are wrong.
set -eu

tag=
while [ $# -gt 0 ]; do
  case $1 in
    --tag)
      [ $# -ge 2 ] || { echo "check-plugin-version: --tag needs a tag" >&2; exit 2; }
      tag=$2
      shift 2
      ;;
    *)
      echo "check-plugin-version: unknown argument $1" >&2
      exit 2
      ;;
  esac
done

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

cargo_toml=Cargo.toml
plugin_json=plugins/claude-dagq/.claude-plugin/plugin.json
marketplace_json=.claude-plugin/marketplace.json

for f in "$cargo_toml" "$plugin_json" "$marketplace_json"; do
  if [ ! -f "$f" ]; then
    echo "check-plugin-version: $f not found under $root" >&2
    exit 2
  fi
done

crate_version=$(awk -F '"' '/^\[package\]/ { p = 1; next }
                            /^\[/ { p = 0 }
                            p && /^version *= */ { print $2; exit }' "$cargo_toml")
plugin_version=$(sed -n 's/^ *"version" *: *"\([^"]*\)".*/\1/p' "$plugin_json" | head -n 1)

if [ -z "$crate_version" ]; then
  echo "check-plugin-version: no [package] version in $cargo_toml" >&2
  exit 2
fi
if [ -z "$plugin_version" ]; then
  echo "check-plugin-version: no version in $plugin_json" >&2
  exit 2
fi

status=0

if [ "$plugin_version" != "$crate_version" ]; then
  echo "check-plugin-version: $plugin_json has version $plugin_version but $cargo_toml has $crate_version" >&2
  status=1
fi

if [ -n "$tag" ]; then
  tag_version=${tag#v}
  if [ "$tag_version" = "$tag" ]; then
    echo "check-plugin-version: --tag $tag is not a v<version> tag" >&2
    exit 2
  fi
  if [ "$crate_version" != "$tag_version" ]; then
    echo "check-plugin-version: tag $tag declares version $tag_version but $cargo_toml has $crate_version" >&2
    status=1
  fi
  if [ "$plugin_version" != "$tag_version" ]; then
    echo "check-plugin-version: tag $tag declares version $tag_version but $plugin_json has $plugin_version" >&2
    status=1
  fi
fi

# The marketplace has the one claude-dagq entry; its `ref` is the only one in
# the file.
case $crate_version in
  *[!0-9.]*)
    echo "check-plugin-version: $crate_version is a development version; the marketplace ref is checked on a release version"
    ;;
  *)
    ref=$(sed -n 's/.*"ref" *: *"\([^"]*\)".*/\1/p' "$marketplace_json" | head -n 1)
    if [ -z "$ref" ]; then
      echo "check-plugin-version: $marketplace_json has no ref in the claude-dagq entry; release $crate_version needs ref v$crate_version" >&2
      status=1
    elif [ "$ref" != "v$crate_version" ]; then
      echo "check-plugin-version: $marketplace_json has ref $ref but release $crate_version needs ref v$crate_version" >&2
      status=1
    fi
    ;;
esac

if [ "$status" -eq 0 ]; then
  echo "check-plugin-version: $cargo_toml and $plugin_json have version $crate_version"
fi

exit "$status"
