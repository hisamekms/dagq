#!/bin/sh
# Check the relative links of the Markdown documents: every .md under docs/
# and every .md at the repository root. Each inline link or image
# `[text](target)` or `![alt](target)` whose target is a relative path must
# name a file or a directory that exists in the repository, resolved from the
# document's directory (a target starting with "/" from the repository root).
#
# What is not checked:
# - external targets (a scheme such as https:, http:, mailto:) and targets
#   that are only an anchor ("#section");
# - the anchor part of "path#anchor" (only the path part is checked), and a
#   "?query" part;
# - links inside fenced code blocks (``` or ~~~) and inline code spans
#   (`...`), which are examples, not links;
# - reference-style links ("[text][ref]" with "[ref]: target"), and
#   autolinks ("<https://...>").
# Known limits, kept for a short script: indented code blocks (4 spaces) are
# read as text, so a broken link in one fails; a target is cut at its first
# ")" (so "[t](a_(b).md)" reads "a_(b"); and a target with ".." is not kept
# inside the repository.
# A target in angle brackets ("[t](<a b.md>)") is read without them, and an
# optional title ("[t](a.md "title")") is dropped. Percent-encoded spaces
# ("%20") are decoded.
#
# The one exception: the links of the ADRs (docs/adr/) to removed documents,
# which the ADRs keep since they are append-only (docs/development/
# documents.md, section "ADR"), so those links are not rewritten and are
# skipped here:
# - docs/journal/ and anything under it, removed after it was frozen
#   (ADR-0008 links two journal entries);
# - each path of the list of removed docs/design documents below
#   ($removed_design), as resolved from the ADR's directory (an ADR writes
#   "../design/<name>.md").
# A link of any other document to them, and a link of an ADR to any other
# missing path, fails like any other; a link to a listed path that exists
# resolves as usual.
#
# The list of removed docs/design documents is the one place of these paths.
# A row is "path | successor | state": path from the repository root,
# successor where the content went (a path, or words such as "the doc
# comment by the definition"), and state one of two:
# - migrating (no state written): the document is about to be removed or was
#   just removed, and the rows still settle. The path may exist or not; only
#   the exception of the ADRs' links holds. A plan adds the path of a
#   document it is to remove as a migrating row, with its successor, in a
#   task that lands on main before the task that removes the document, and
#   the removing task depends on it: the program review of a run copies this
#   script from the landing branch (main), so a row added in the removing
#   commit does not hold in that review, and the ADRs' links to the removed
#   document fail there.
# - final ("final"): the removal is settled and the document must not be
#   made again at the path; its content goes to the successor. The path
#   existing fails, naming the successor.
# Only the last task of the goal that removed the document turns its row from
# migrating to final. A final row without a successor, two rows of one path,
# and a state other than empty or "final" are errors of the list and fail.
#
# The tree checked is the git work tree of the cwd (`git rev-parse
# --show-toplevel`), so a copy of the script run elsewhere with the cwd in a
# worktree checks that worktree (the program review of a run;
# docs/development/task-registration.md, section "推奨の組み合わせ").
# Outside a git work tree it is the repository found from the script's own
# location. Run it from the repository root (`sh scripts/check-doc-links.sh`).
#
# Exit 0 when every relative link resolves and the list holds, 1 when a link
# does not resolve (each file, line number and target go to stderr) or the
# list fails (each row and why), 2 when docs/ is not found.
set -eu

# The list of removed docs/design documents: "path | successor | state" (see
# the header). Blank lines and lines starting with "#" are skipped.
removed_design=$(cat <<'EOF'
docs/design/broker.md | docs/adr/2026-10-08-t2113-1-remove-the-resource-broker.md | final
docs/design/supervisor-lifecycle/task-hold.md | docs/plans/task-hold.md | final
docs/design/supervisor-lifecycle/task-replanning.md | docs/plans/task-replanning.md | final
docs/design/manual-smoke.md | docs/development/manual-smoke.md (the procedures) and docs/plans/manual-smoke-results.md (the dated results) | final
docs/design/supervisor-lifecycle/kpi.md | docs/design/supervisor-lifecycle/commands.md and the doc comments of domain::kpi | final
docs/design/supervisor-lifecycle/stats.md | docs/design/supervisor-lifecycle/commands.md and the doc comments of domain::stats | final
docs/design/supervisor-lifecycle/marks.md | docs/design/supervisor-lifecycle/commands.md and the doc comments of domain::marks | final
docs/design/supervisor-lifecycle/report.md | docs/design/supervisor-lifecycle/commands.md and the doc comments of domain::kpi::report and application::report | final
docs/design/supervisor-lifecycle/timeline.md | docs/design/supervisor-lifecycle/commands.md and the doc comments of domain::timeline | final
docs/design/supervisor-lifecycle/prompt-waiting.md | docs/adr/2026-10-03-t1433-2-abolish-the-interactive-route.md | final
docs/design/supervisor-lifecycle/session-send.md | docs/design/authorization.md | final
docs/design/supervisor-lifecycle/implementation-status.md | docs/design/supervisor-lifecycle/receipt-and-session-exit.md | final
docs/design/supervisor-lifecycle/notification-route.md | docs/design/supervisor-lifecycle/ask-push.md | final
docs/design/supervisor-lifecycle/cmux-notify.md | docs/design/supervisor-lifecycle/ask-push.md | final
docs/design/supervisor-lifecycle/naming.md | docs/design/supervisor-lifecycle/headless-worker.md | final
docs/design/supervisor-lifecycle/run-workspaces.md | docs/design/supervisor-lifecycle/cleanup-and-recovery.md | final
docs/design/supervisor-lifecycle/session-prompts.md | docs/design/supervisor-lifecycle/roles.md | final
docs/design/supervisor-lifecycle/worker-question-answer.md | docs/design/supervisor-lifecycle/receipt-and-session-exit.md | final
docs/design/supervisor-lifecycle/silent-wrapper.md | docs/design/supervisor-lifecycle/receipt-and-session-exit.md | final
docs/design/supervisor-lifecycle/first-commit.md | the doc comment of watch_first_commit in src/application/supervise/session.rs | final
docs/design/supervisor-lifecycle/worktree-read-only-git.md | the doc comments of GitRepository in src/infrastructure/adapters.rs | final
docs/design/slow-tests.md | docs/development/testing.md and the headers of scripts/slow-tests.sh and scripts/check-it-test-time.sh | final
docs/design/stress-ci.md | docs/development/testing.md and the headers of .github/workflows/stress.yml and scripts/stress-recent-tests.sh | final
docs/design/linux-ci.md | docs/development/testing.md and the comments of .github/workflows/ci.yml | final
docs/design/ci-failure-issues.md | docs/development/testing.md and the header of .github/workflows/ci-failure.yml | final
docs/design/test-fixtures.md | docs/development/testing.md and the doc comment of tests/common/template.rs | final
docs/design/supervisor-lifecycle/status.md | docs/design/supervisor-lifecycle/commands.md | final
docs/design/supervisor-lifecycle/events-watch.md | docs/design/supervisor-lifecycle/commands.md | final
docs/design/supervisor-lifecycle/doctor.md | docs/design/supervisor-lifecycle/commands.md | final
docs/design/supervisor-lifecycle/logs.md | docs/design/supervisor-lifecycle/commands.md | final
docs/design/supervisor-lifecycle/review-command.md | docs/design/supervisor-lifecycle/commands.md | final
docs/design/supervisor-lifecycle/recover.md | docs/design/supervisor-lifecycle/commands.md | final
docs/design/supervisor-lifecycle/rebind.md | docs/design/supervisor-lifecycle/commands.md | final
docs/design/supervisor-lifecycle/dependency-diagram.md | docs/design/supervisor-lifecycle/commands.md | final
docs/design/supervisor-lifecycle/conflict-thresholds.md | the doc comments by the definitions | final
docs/design/supervisor-lifecycle/stall-thresholds.md | the doc comments by the definitions | final
docs/design/supervisor-lifecycle/claim-defer.md | docs/design/supervisor-lifecycle/claim-hold.md |
docs/design/supervisor-lifecycle/disk-space.md | docs/design/supervisor-lifecycle/claim-hold.md |
docs/design/supervisor-lifecycle/landing-recheck.md | docs/design/supervisor-lifecycle/integrate.md |
docs/design/supervisor-lifecycle/landing-branch.md | docs/design/supervisor-lifecycle/integrate.md |
docs/design/supervisor-lifecycle/landing-e2e.md | docs/design/supervisor-lifecycle/integrate.md |
docs/design/supervisor-lifecycle/integrate-errors.md | docs/design/supervisor-lifecycle/integrate.md |
docs/design/supervisor-lifecycle/abandon.md | docs/design/supervisor-lifecycle/background-recovery-job.md |
docs/design/supervisor-lifecycle/idle-without-receipt.md | docs/design/supervisor-lifecycle/background-recovery-job.md |
docs/design/supervisor-lifecycle/cleanup-and-recovery.md | docs/design/supervisor-lifecycle/background-recovery-job.md |
docs/design/supervisor-lifecycle/session-wrapper.md | docs/design/supervisor-lifecycle/headless-worker.md |
docs/design/supervisor-lifecycle/receipt-and-session-exit.md | docs/design/supervisor-lifecycle/headless-worker.md |
EOF
)

root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

if [ ! -d docs ]; then
  echo "check-doc-links: docs not found under $root" >&2
  exit 2
fi

# One message per error of the list, and one per final row whose path exists.
list_bad=$(printf '%s\n' "$removed_design" | awk -F'|' '
  function trim(v) { sub(/^[ \t]+/, "", v); sub(/[ \t]+$/, "", v); return v }
  /^[ \t]*(#|$)/ { next }
  {
    row = trim($0)
    p = trim($1); to = trim($2); st = trim($3)
    if (NF > 3) { print "list: more than 3 fields: " row; next }
    if (p !~ /^docs\/./) { print "list: path not under docs/: " row; next }
    if (p ~ /\/\/|\/\.\.?\/|\/$/) { print "list: path not written plainly (no //, /./, /../ or trailing /): " row; next }
    if (p in seen) print "list: two rows of " p
    seen[p] = 1
    if (st != "" && st != "final") { print "list: state is neither empty (migrating) nor final: " row; next }
    if (st == "final" && to == "") { print "list: final row without a successor: " row; next }
    if (st == "final") printf "%s\t%s\n", p, to
  }
' | while IFS='	' read -r p to; do
  case $p in
    (list:*) printf '%s\n' "$p" ;;
    (*) [ ! -e "$p" ] || printf '%s: made again after its removal was made final; write it in %s\n' "$p" "$to" ;;
  esac
done)
removed_paths=$(printf '%s\n' "$removed_design" | awk -F'|' '!/^[ \t]*(#|$)/ { p = $1; gsub(/^[ \t]+|[ \t]+$/, "", p); print p }')
export removed_paths

# One "file<TAB>line<TAB>target<TAB>path" per relative link, where path is
# the target joined to the document's directory.
links=$({
  find docs -type f -name '*.md'
  find . -maxdepth 1 -type f -name '*.md' | sed 's|^\./||'
} | LC_ALL=C sort | while IFS= read -r f; do
  awk -v file="$f" '
    # The path with "." and "x/.." taken out.
    function norm(p,   n, a, s, i, k, out) {
      n = split(p, a, "/")
      k = 0
      for (i = 1; i <= n; i++) {
        if (a[i] == "" || a[i] == ".") continue
        if (a[i] == ".." && k > 0 && s[k] != "..") { k--; continue }
        s[++k] = a[i]
      }
      out = ""
      for (i = 1; i <= k; i++) out = out (i > 1 ? "/" : "") s[i]
      return out
    }
    BEGIN {
      dir = file; if (!sub(/\/[^\/]*$/, "", dir)) dir = "."
      n = split(ENVIRON["removed_paths"], r, "\n")
      for (i = 1; i <= n; i++) if (r[i] != "") removed[r[i]] = 1
    }
    {
      line = $0
      sub(/\r$/, "", line)
      # A fence opens with 3 or more ` or ~ and closes with the same
      # character, at least as many, and nothing after them.
      if (match(line, /^ ? ? ?(```+|~~~+)/)) {
        f = substr(line, 1, RLENGTH)
        sub(/^ */, "", f)
        rest = substr(line, RLENGTH + 1)
        if (infence == "") { infence = f; next }
        if (substr(f, 1, 1) == substr(infence, 1, 1) && length(f) >= length(infence) && rest ~ /^[ \t]*$/) {
          infence = ""
          next
        }
      }
      if (infence != "") next
      # Drop inline code spans.
      gsub(/`[^`]*`/, "", line)
      while (match(line, /\]\([^)]*\)/)) {
        target = substr(line, RSTART + 2, RLENGTH - 3)
        line = substr(line, RSTART + RLENGTH)
        sub(/^[ \t]+/, "", target)
        if (target ~ /^</) {
          sub(/^</, "", target)
          sub(/>.*$/, "", target)
        } else {
          sub(/[ \t].*$/, "", target)
        }
        if (target == "" || target ~ /^#/ || target ~ /^[A-Za-z][A-Za-z0-9+.-]*:/) continue
        sub(/#.*$/, "", target)
        sub(/\?.*$/, "", target)
        gsub(/%20/, " ", target)
        if (target == "") continue
        if (target ~ /^\//) path = "." target
        else path = dir "/" target
        # The ADRs are append-only, so their links to the removed
        # docs/journal/ and the listed removed documents stay (see the header).
        if (file ~ /^docs\/adr\//) {
          np = norm(path)
          if (np == "docs/journal" || np ~ /^docs\/journal\// || (np in removed)) continue
        }
        printf "%s\t%d\t%s\t%s\n", file, FNR, target, path
      }
    }
  ' "$f"
done)

bad=$(printf '%s\n' "$links" | while IFS='	' read -r f n target path; do
  [ -n "$f" ] || continue
  [ -e "$path" ] || printf '%s:%s: %s\n' "$f" "$n" "$target"
done)

status=0
if [ -n "$list_bad" ]; then
  echo "check-doc-links: the list of removed docs/design documents fails:" >&2
  printf '%s\n' "$list_bad" | sed 's/^/  /' >&2
  status=1
fi
if [ -n "$bad" ]; then
  echo "check-doc-links: relative links must name a file or directory in the repository:" >&2
  printf '%s\n' "$bad" | sed 's/^/  /' >&2
  status=1
fi
exit "$status"
