#!/bin/sh
# Check that the check scripts check the tree of the cwd and not the place the
# script is in. The program review of a run writes a script from the landing
# branch to a temporary place and runs it with the cwd in the run's worktree,
# so each script must take its root from `git rev-parse --show-toplevel` of
# the cwd (docs/development/task-registration.md, section "推奨の組み合わせ").
#
# It makes a temporary clone of HEAD (`git clone --shared` under ${TMPDIR};
# changes and tags in the clone do not touch this repository), copies each
# script in $scripts to a temporary place outside the clone, and checks:
#
# (a) each copied script exits 0 with the cwd at the root of the unchanged
#     clone;
# (b) for each case in $cases, one violation put only into the clone makes the
#     copied script exit 1, while the same script run from this repository's
#     root still exits 0;
# (c) a migration of a temporary release tag after v0.3.0 made in the clone,
#     once changed, makes check-migration-numbers.sh exit 1, and passing that
#     tag to --release takes it out of the base so it exits 0;
# (d) with the cwd outside any git work tree, a copied script falls back to
#     its own location and exits 2 (nothing to check there). Skipped when
#     ${TMPDIR} is itself inside a git work tree.
#
# A task that adds a check script, or changes how one finds its root, adds the
# script to $scripts and at least one case to $cases with its violate_<case>
# function below, so the list of scripts and how to break each stay here.
#
# Usage: sh scripts/check-scripts-root.sh
#
# Exit 0 when every case behaves as expected, 1 when a case does not (the
# script, the case and its output go to stderr), 2 when the clone or the
# fixtures cannot be prepared.
set -u

me=check-scripts-root

# The check scripts that take their root from the cwd's git work tree.
scripts="check-migration-numbers check-test-file-lines check-adr-numbers check-e2e-quarantine check-agents-md-size check-layer-deps check-frontmatter-dates check-design-docs check-doc-links check-doc-frontmatter"

# "<script> <case>": each case puts one violation of the script into the
# clone (the cwd) with violate_<case>.
cases="check-migration-numbers migration_gap
check-test-file-lines test_file_too_long
check-adr-numbers adr_id_mismatch
check-e2e-quarantine e2e_quarantine_malformed
check-agents-md-size agents_md_too_big
check-layer-deps layer_forbidden_reference
check-layer-deps layer_stale_allow_item
check-layer-deps context_reaches_loop_state
check-frontmatter-dates frontmatter_date_comment
check-design-docs design_doc_too_big
check-doc-links doc_broken_link
check-doc-links doc_link_to_removed_design
check-doc-frontmatter doc_frontmatter_missing_key
check-doc-frontmatter doc_frontmatter_updated_line"

violate_migration_gap() {
  echo 'SELECT 1;' >migrations/9999_scripts_root_gap.sql
}

violate_test_file_too_long() {
  awk 'BEGIN { for (i = 1; i <= 3001; i++) print "// line " i }' >tests/zz_scripts_root_long.rs
}

violate_adr_id_mismatch() {
  printf -- '---\nid: adr-0001\nstatus: proposed\n---\n\n# fixture\n' >docs/adr/9999-scripts-root-fixture.md
}

violate_e2e_quarantine_malformed() {
  mkdir -p .config
  printf '[[test]]\nname = "no_such_test"\n' >.config/e2e-quarantine.toml
}

violate_agents_md_too_big() {
  awk 'BEGIN { for (i = 1; i <= 200; i++) print "padding line for check-scripts-root, over the size limit" }' >>AGENTS.md
}

violate_layer_forbidden_reference() {
  echo 'use crate::infrastructure::scripts_root_fixture;' >src/domain/zz_scripts_root.rs
}

violate_layer_stale_allow_item() {
  mkdir -p .config
  echo 'L1 | src/domain/zz_scripts_root.rs | crate::infrastructure | 1899 | fixture' >>.config/layer-deps-allow.txt
}

violate_context_reaches_loop_state() {
  echo "impl Supervisor<'_> { fn zz_scripts_root(&self) -> usize { self.slots.len() } }" >>src/application/supervise/report.rs
}

violate_frontmatter_date_comment() {
  printf -- '---\nupdated: 2026-10-07 # task 1899\n---\n\n# fixture\n' >docs/zz-scripts-root-fixture.md
}

violate_design_doc_too_big() {
  awk 'BEGIN { print "# fixture"; for (i = 1; i <= 500; i++) print "padding line for check-scripts-root, over the size budget of a design document" }' >docs/design/zz-scripts-root-fixture.md
}

violate_doc_broken_link() {
  printf -- '---\nid: zz-scripts-root-fixture\n---\n\n[fixture](zz-no-such-file.md)\n' >docs/zz-scripts-root-link.md
}

# The exception of check-doc-links for docs/design/broker.md holds only for
# the ADRs: a link of any other document to it fails. The file is removed in
# the clone too, so the case holds whether or not HEAD still has it.
violate_doc_link_to_removed_design() {
  rm -f docs/design/broker.md
  printf -- '---\nid: zz-scripts-root-removed\n---\n\n[fixture](design/broker.md)\n' >docs/zz-scripts-root-removed.md
}

violate_doc_frontmatter_missing_key() {
  printf -- '---\nid: zz-scripts-root-fixture\ntype: plan\nstatus: active\ncreated: 2026-10-07\n---\n\n# fixture\n' >docs/zz-scripts-root-frontmatter.md
}

violate_doc_frontmatter_updated_line() {
  printf -- '---\nid: zz-scripts-root-updated\ntype: design\ntitle: fixture\nstatus: current\ncreated: 2026-10-07\nupdated: 2026-10-07\n---\n\n# fixture\n' >docs/design/zz-scripts-root-updated.md
}

unset LAYER_DEPS_ROOT LAYER_DEPS_ALLOW_FILE E2E_QUARANTINE_FILE
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_COMMON_DIR GIT_ALTERNATE_OBJECT_DIRECTORIES

prep_fail() {
  echo "$me: $*" >&2
  exit 2
}

repo=$(git rev-parse --show-toplevel 2>/dev/null) || repo=$(cd "$(dirname "$0")/.." && pwd)
[ -d "$repo/scripts" ] || prep_fail "scripts not found under $repo"

tmp_parent=${TMPDIR:-/tmp}
tmp=$(mktemp -d "${tmp_parent%/}/$me.XXXXXX") || prep_fail "cannot make a temporary directory under $tmp_parent"
trap 'rm -rf "$tmp"' EXIT
tmp=$(cd "$tmp" && pwd) || prep_fail "cannot resolve $tmp"
trap 'exit 2' INT TERM
clone=$tmp/clone
bin=$tmp/bin
outside=$tmp/outside

git clone --shared --quiet "$repo" "$clone" || prep_fail "cannot clone $repo"
mkdir -p "$bin" "$outside" || prep_fail "cannot make $bin"
for s in $scripts; do
  cp "$repo/scripts/$s.sh" "$bin/$s.sh" || prep_fail "cannot copy scripts/$s.sh"
done

# Put the clone back to HEAD with no untracked file.
reset_clone() {
  git -C "$clone" reset --quiet --hard HEAD && git -C "$clone" clean --quiet -fdx ||
    prep_fail "cannot reset the clone"
}

fail=0
# expect <want-exit> <label> <dir> <script path> [args...]
expect() {
  want=$1 label=$2 dir=$3
  shift 3
  got=0
  (cd "$dir" && sh "$@") </dev/null >"$tmp/out" 2>&1 || got=$?
  if [ "$got" -ne "$want" ]; then
    echo "$me: $label: exit $got, want $want" >&2
    sed 's/^/  /' "$tmp/out" >&2
    fail=1
  else
    echo "$me: $label: exit $got as expected"
  fi
}

# (a) The unchanged clone.
for s in $scripts; do
  expect 0 "$s on the unchanged clone" "$clone" "$bin/$s.sh"
done

# (b) One violation only in the clone.
while read -r s c; do
  [ -n "$s" ] || continue
  case " $scripts " in
    *" $s "*) ;;
    *)
      echo "$me: case $c names $s, which is not in \$scripts" >&2
      fail=1
      continue
      ;;
  esac
  (cd "$clone" && "violate_$c") </dev/null || prep_fail "cannot put $c into the clone"
  # Nothing is committed: the scripts read the working tree.
  expect 1 "$s with $c in the clone" "$clone" "$bin/$s.sh"
  expect 0 "$s in $repo while the clone has $c" "$repo" "$repo/scripts/$s.sh"
  reset_clone
done <<EOF
$cases
EOF

# (c) A released migration changed in the clone. The tag is on a commit that
# adds a migration of its own, so --release takes every changed file out of
# the base whatever release tags the clone has.
tag=v999.0.0
last=$(cd "$clone" && ls migrations/[0-9][0-9][0-9][0-9]_?*.sql | sed -n '$s|^migrations/\([0-9]*\)_.*|\1|p')
[ -n "$last" ] || prep_fail "no migration in the clone"
next=$(printf '%04d' $(($(echo "$last" | sed 's/^0*//') + 1)))
released=migrations/${next}_scripts_root_released.sql
(
  cd "$clone" &&
    echo 'SELECT 1;' >"$released" &&
    git add "$released" &&
    git -c user.name="$me" -c user.email="$me@localhost" -c commit.gpgsign=false \
      commit --quiet --no-verify -m "$me fixture" &&
    git tag "$tag"
) || prep_fail "cannot make the release tag $tag in the clone"
expect 0 "check-migration-numbers with the clone at $tag" "$clone" "$bin/check-migration-numbers.sh"
echo 'SELECT 2;' >"$clone/$released" || prep_fail "cannot change $released in the clone"
expect 1 "check-migration-numbers with $released of $tag changed" "$clone" "$bin/check-migration-numbers.sh"
expect 0 "check-migration-numbers --release $tag with $released changed" "$clone" "$bin/check-migration-numbers.sh" --release "$tag"
expect 0 "check-migration-numbers in $repo while the clone has $tag" "$repo" "$repo/scripts/check-migration-numbers.sh"
if git -C "$repo" rev-parse --quiet --verify "refs/tags/$tag" >/dev/null 2>&1; then
  echo "$me: the tag $tag of the clone is in $repo" >&2
  fail=1
fi

# (d) The cwd outside any git work tree.
if git -C "$outside" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "$me: $outside is inside a git work tree; fallback to the script's location not checked"
else
  for s in $scripts; do
    expect 2 "$s with the cwd outside a git work tree" "$outside" "$bin/$s.sh"
  done
fi

if [ "$fail" -ne 0 ]; then
  exit 1
fi
echo "$me: ok"
