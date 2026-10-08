#!/bin/sh
# Check that the plugin and the binary carry the same version, and that the
# marketplace offers the plugin of a release tag (ADR-t617-1).
#
# - Always: the `version` of plugins/claude-dagq/.claude-plugin/plugin.json is
#   the `[package].version` of Cargo.toml (decision 3; on main both are
#   X.Y.Z-dev, in a release both are X.Y.Z).
# - Always: the claude-dagq entry of .claude-plugin/marketplace.json has one of
#   two forms. Either the source is the relative path "./plugins/claude-dagq"
#   (the form before the first pinned release, decision 6), or it is
#     {"source": "git-subdir", "url": "hisamekms/dagq",
#      "path": "plugins/claude-dagq", "ref": "v<X.Y.Z>"}
#   and the entry has no `version` (decisions 2 and 3).
# - When the version is a release (X.Y.Z, no pre-release part): the entry is
#   the git-subdir form with `ref` vX.Y.Z, the tag the release commit is about
#   to get (decision 4). On a -dev version the entry may keep the ref of the
#   previous release, or be the relative path before the first pinned release
#   (decision 6), so the ref is not compared.
# - With a tag vX.Y.Z (--tag, or GITHUB_REF_NAME when GITHUB_REF_TYPE is tag;
#   release.yml): the version is also X.Y.Z of the tag, and the entry is the
#   git-subdir form whose ref is that tag, so the tag's marketplace points at
#   the tag itself (decision 4).
#
# Usage: sh scripts/check-plugin-version.sh [--tag vX.Y.Z]
#
# It checks the git work tree of the cwd (`git rev-parse --show-toplevel`), so
# a copy of the script run elsewhere with the cwd in a worktree checks that
# worktree; only outside any git work tree does it fall back to the
# repository root found from the script's own location. Run it from the
# repository root (`sh scripts/check-plugin-version.sh`). The marketplace
# JSON is read with python3, which macOS and the GitHub runners have.
#
# Exit 0 when everything agrees, 1 when something does not (each mismatch goes
# to stderr with the file and the values), 2 when a file, a version or python3
# is not found or the arguments are wrong.
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
if [ -z "$tag" ] && [ "${GITHUB_REF_TYPE:-}" = tag ]; then
  tag=${GITHUB_REF_NAME:-}
fi

root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$(dirname "$0")/.." && pwd)
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
if ! command -v python3 >/dev/null 2>&1; then
  echo "check-plugin-version: python3 is needed to read $marketplace_json" >&2
  exit 2
fi

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
    echo "check-plugin-version: tag $tag is not a v<version> tag" >&2
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

release=
case $crate_version in
  *[!0-9.]*)
    echo "check-plugin-version: $crate_version is a development version; the marketplace ref is compared on a release version"
    ;;
  *) release=$crate_version ;;
esac

set +e
python3 - "$marketplace_json" "$release" "$tag" <<'PY'
import json
import re
import sys

path, release, tag = sys.argv[1], sys.argv[2], sys.argv[3]
name = "claude-dagq"
relative = "./plugins/claude-dagq"
expected = {"source": "git-subdir", "url": "hisamekms/dagq", "path": "plugins/claude-dagq"}
errors = []


def fail(message):
    errors.append("check-plugin-version: %s: the %s entry %s" % (path, name, message))


try:
    with open(path) as f:
        marketplace = json.load(f)
except (OSError, ValueError) as e:
    print("check-plugin-version: %s is not readable JSON: %s" % (path, e), file=sys.stderr)
    sys.exit(2)

plugins = marketplace.get("plugins") if isinstance(marketplace, dict) else None
entries = [p for p in plugins or [] if isinstance(p, dict) and p.get("name") == name]
if len(entries) != 1:
    errors.append("check-plugin-version: %s has %d %s entries; it needs exactly one"
                  % (path, len(entries), name))
else:
    entry = entries[0]
    source = entry.get("source")
    if isinstance(source, str):
        if source != relative:
            fail("has source %s; a path source must be %s" % (json.dumps(source), json.dumps(relative)))
        if tag:
            fail("has the relative source %s; tag %s needs a git-subdir source with ref %s"
                 % (json.dumps(source), tag, tag))
        elif release:
            fail("has the relative source %s and no ref; release %s needs a git-subdir source with ref v%s"
                 % (json.dumps(source), release, release))
    elif isinstance(source, dict):
        for key, value in expected.items():
            if source.get(key) != value:
                fail("has source.%s %s but needs %s"
                     % (key, json.dumps(source.get(key)), json.dumps(value)))
        ref = source.get("ref")
        if not isinstance(ref, str) or not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", ref):
            fail("has source.ref %s but needs a release tag v<X.Y.Z>" % json.dumps(ref))
        if "version" in entry:
            fail("has version %s; the version is the tag's plugin.json, so the entry must not have one"
                 % json.dumps(entry["version"]))
        if tag:
            if ref != tag:
                fail("has source.ref %s but tag %s needs ref %s" % (json.dumps(ref), tag, tag))
        elif release and ref != "v" + release:
            fail("has source.ref %s but release %s needs ref v%s" % (json.dumps(ref), release, release))
    else:
        fail("has source %s; it needs %s or a git-subdir source"
             % (json.dumps(source), json.dumps(relative)))

for error in errors:
    print(error, file=sys.stderr)
sys.exit(1 if errors else 0)
PY
marketplace_status=$?
set -e
case $marketplace_status in
  0) ;;
  1) status=1 ;;
  *) exit 2 ;;
esac

if [ "$status" -eq 0 ]; then
  echo "check-plugin-version: $cargo_toml and $plugin_json have version $crate_version, and the $marketplace_json entry agrees"
fi

exit "$status"
