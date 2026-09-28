#!/bin/sh
# Check that the migrations under migrations/ are named NNNN_<name>.sql and
# numbered from 0001 without a gap or a repeat (ADR-0067). build.rs refuses
# the same numbers when it lists the migrations; this names them without a
# build, for CI and a task's verification.
#
# It also checks that the migrations of the latest release are unchanged
# (ADR-t614-2): every migrations/*.sql in the latest v<X.Y.Z> tag must still be
# in the working tree under the same name with the same bytes. The rule starts
# with the first release after ADR-t614-2, so tags up to v0.3.0 are not a base
# (decision 3). A clone without such a tag (a shallow clone, a checkout that
# did not fetch tags) says so and passes this part.
#
# It also refuses a CHECK constraint in a migration numbered after the one
# that dropped them all, migrations/NNNN_no_check_constraints.sql
# (ADR-t876-1): until the schema is stable the rules live in the domain and
# the write port, not in the queue. SQL comments are not read; the released
# migrations and the ones up to that file are not checked.
#
# Usage: sh scripts/check-migration-numbers.sh [--release vX.Y.Z]
#   --release TAG  compare with the latest release tag before TAG instead of
#                  the latest one (release.yml, which runs on TAG itself).
#
# Meant to be run from the repository root (`sh scripts/check-migration-numbers.sh`).
# When run from anywhere else it changes to the repository root found from the
# script's own location, so the result does not depend on the cwd.
#
# Exit 0 when the numbers are fine and the released migrations are unchanged,
# 1 when a file is misnamed, a number is shared or missing, a migration after
# the one that dropped the CHECKs has one, or a released migration was
# changed, renamed or removed (the offending files go to
# stderr), 2 when migrations/ is not found or the arguments are wrong.
set -eu

release=
while [ $# -gt 0 ]; do
  case $1 in
    --release)
      [ $# -ge 2 ] || { echo "check-migration-numbers: --release needs a tag" >&2; exit 2; }
      release=$2
      shift 2
      ;;
    *)
      echo "check-migration-numbers: unknown argument $1" >&2
      exit 2
      ;;
  esac
done

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

if [ ! -d migrations ]; then
  echo "check-migration-numbers: migrations not found under $root" >&2
  exit 2
fi

status=0

# Misnamed .sql files.
for f in migrations/*.sql; do
  [ -e "$f" ] || continue
  case $(basename "$f") in
    [0-9][0-9][0-9][0-9]_?*.sql) ;;
    *)
      echo "check-migration-numbers: $f is not named NNNN_<name>.sql" >&2
      status=1
      ;;
  esac
done

numbers=$(
  for f in migrations/[0-9][0-9][0-9][0-9]_?*.sql; do
    [ -e "$f" ] || continue
    basename "$f" | cut -c1-4
  done | sort
)

# Duplicate numbers.
for n in $(printf '%s\n' "$numbers" | uniq -d); do
  echo "check-migration-numbers: migration number $n is used by more than one file:" >&2
  for f in migrations/"$n"_*.sql; do
    echo "  $f" >&2
  done
  status=1
done

# Numbers run from 0001 without a gap.
expected=1
for n in $(printf '%s\n' "$numbers" | uniq); do
  value=$(printf '%s' "$n" | sed 's/^0*//')
  value=${value:-0}
  if [ "$value" -ne "$expected" ]; then
    if [ "$value" -lt "$expected" ]; then
      echo "check-migration-numbers: migration number $n comes before 0001" >&2
    else
      echo "check-migration-numbers: migration number $(printf '%04d' "$expected") is missing before $(ls migrations/"$n"_*.sql | head -n 1)" >&2
    fi
    status=1
  fi
  if [ "$value" -ge "$expected" ]; then
    expected=$((value + 1))
  fi
done

# No CHECK after the migration that dropped them all (ADR-t876-1).
dropped=$(ls migrations/[0-9][0-9][0-9][0-9]_no_check_constraints.sql 2>/dev/null | head -n 1)
if [ -z "$dropped" ]; then
  echo "check-migration-numbers: migrations/NNNN_no_check_constraints.sql not found; cannot check for CHECK constraints" >&2
  status=1
else
  last=$(basename "$dropped" | cut -c1-4 | sed 's/^0*//')
  for f in migrations/[0-9][0-9][0-9][0-9]_?*.sql; do
    [ -e "$f" ] || continue
    n=$(basename "$f" | cut -c1-4 | sed 's/^0*//')
    [ "${n:-0}" -gt "$last" ] || continue
    lines=$(sed 's/--.*$//' "$f" | grep -n -i -w 'check' | cut -d: -f1 | tr '\n' ' ' || true)
    if [ -n "$lines" ]; then
      echo "check-migration-numbers: $f has a CHECK constraint (line ${lines% }); the queue has none after $dropped (ADR-t876-1), so keep the rule in the domain and the write port" >&2
      status=1
    fi
  done
fi

# Released migrations (ADR-t614-2).

# The version of a release tag vX.Y.Z as one comparable number, or nothing for
# any other tag.
tag_key() {
  printf '%s\n' "$1" | sed -n 's/^v\([0-9]\{1,\}\)\.\([0-9]\{1,\}\)\.\([0-9]\{1,\}\)$/\1 \2 \3/p' |
    awk '{ printf "%d%06d%06d\n", $1, $2, $3 }'
}

# Tags up to v0.3.0 predate ADR-t614-2 and are not a base (decision 3).
first_key=$(tag_key v0.3.0)
limit_key=
if [ -n "$release" ]; then
  limit_key=$(tag_key "$release")
  if [ -z "$limit_key" ]; then
    echo "check-migration-numbers: --release $release is not a vX.Y.Z tag" >&2
    exit 2
  fi
fi

base=
base_key=
if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  for t in $(git tag --list 'v*'); do
    key=$(tag_key "$t")
    [ -n "$key" ] || continue
    [ "$key" -gt "$first_key" ] || continue
    if [ -n "$limit_key" ] && [ "$key" -ge "$limit_key" ]; then
      continue
    fi
    if [ -z "$base_key" ] || [ "$key" -gt "$base_key" ]; then
      base=$t
      base_key=$key
    fi
  done
fi

if [ -z "$base" ]; then
  if [ -n "$release" ]; then
    echo "check-migration-numbers: no release tag after v0.3.0 and before $release; released migrations not checked"
  else
    echo "check-migration-numbers: no release tag after v0.3.0 in this clone; released migrations not checked"
  fi
  exit "$status"
fi

changed=0
released=$(git ls-tree --name-only "$base" migrations/ | grep '\.sql$' || true)
count=0
for f in $released; do
  count=$((count + 1))
  if [ -e "$f" ]; then
    if ! git cat-file blob "$base:$f" | cmp -s - "$f"; then
      echo "check-migration-numbers: $f was released in $base and has changed; add a new migration instead" >&2
      changed=1
    fi
    continue
  fi
  blob=$(git rev-parse "$base:$f")
  renamed=
  for g in migrations/*.sql; do
    [ -e "$g" ] || continue
    if [ "$(git hash-object "$g")" = "$blob" ]; then
      renamed=$g
      break
    fi
  done
  if [ -n "$renamed" ]; then
    echo "check-migration-numbers: $f was released in $base and was renamed to $renamed" >&2
  else
    echo "check-migration-numbers: $f was released in $base and was removed" >&2
  fi
  changed=1
done

if [ "$changed" -eq 0 ]; then
  echo "check-migration-numbers: $count migrations released in $base are unchanged"
else
  status=1
fi

exit "$status"
