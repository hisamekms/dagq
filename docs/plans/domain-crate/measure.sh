#!/bin/sh
# Measure the domain crate prototype (condition B) against the same commit
# without it (condition A): docs/plans/domain-crate-measurement.md.
#
# Usage: sh measure.sh A_DIR B_DIR SECTION [ROUND...]
#   A_DIR    the commit unpacked by `git archive HEAD | tar -x -C A_DIR`,
#            then committed as a repository of its own (`git init`,
#            `git add -A`, `git commit`)
#   B_DIR    the same as A_DIR, with prototype.patch applied and committed
#            (`git apply --index prototype.patch`, `git commit`)
#   SECTION  build-times (1), clean-builds (2) or domain-tests (3)
#   ROUND    the rounds to run (default: all, 1-5 for (1), 1-3 for (2)(3)),
#            so a section can be run in parts; the order inside a round
#            depends only on the round's number. A trial whose line is
#            already in the CSV is skipped, so a stopped run is resumed by
#            running the section again.
#
# Each trial is one CSV line appended to the section's file next to this
# script (build-times.csv, clean-builds.csv, domain-tests.csv); the header is
# written when the file does not exist. A and B keep their own target dirs
# (each tree's target/), so switching does not touch the other's warm
# state. k2 builds in target/llvm-cov-target, the dir `cargo llvm-cov
# nextest` uses: `cargo llvm-cov show-env` (0.9) names target/ and adds the
# instrumentation in its rustc wrapper, which cargo's fingerprint does not
# see, so k1 and k2 in one dir would take each other's artifacts as fresh.
# Both are repositories of their own with a clean tree: dagq's build script watches the Git files (`.git`, the
# branch's loose ref, the index), and a file it names that does not exist
# (no `.git` in a bare unpacked tree, or a run worktree whose branch ref is
# only in packed-refs) reruns it and rebuilds dagq on every command. Run no
# `git` command in A_DIR or B_DIR while measuring: one that refreshes the
# index reruns the build script. The wall is the `real` of
# `/usr/bin/time -p`, and the crates compiled are the lines of the command's
# stderr matching `^ *Compiling `. The stderr of each timed command is kept
# in $LOG_DIR (default $TMPDIR/domain-crate-logs).
set -eu

[ $# -ge 3 ] || { echo "usage: sh measure.sh A_DIR B_DIR SECTION [ROUND...]" >&2; exit 2; }
a_dir=$(cd "$1" && pwd)
b_dir=$(cd "$2" && pwd)
section=$3
shift 3
here=$(cd "$(dirname "$0")" && pwd)
log_dir=${LOG_DIR:-${TMPDIR:-/tmp}/domain-crate-logs}
mkdir -p "$log_dir"

# k1 names the workspace: in B a bare `cargo test` builds only the root
# package and leaves out dagq-domain's unit tests (in A it changes nothing).
k1='cargo test --locked --no-run --workspace'
k2='cargo nextest run --locked --workspace --no-run'

now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

dir_of() { case $1 in A) echo "$a_dir" ;; *) echo "$b_dir" ;; esac; }

# The file a change touches, relative to the tree of the condition.
file_of() {
  case $2 in
    a) case $1 in A) echo src/domain/stall.rs ;; *) echo crates/dagq-domain/src/stall.rs ;; esac ;;
    b) echo src/application/areas.rs ;;
    c) echo src/lib.rs ;;
  esac
}

# A change appends the marker line to a file; reverting copies its original
# back. The original and the file it belongs to ("DIR<TAB>FILE") are kept in
# $log_dir, outside the trees, until the revert, so a run that stops between
# the two (an error, an interrupt, a kill) is restored on its exit or, failing
# that, at the start of the next run. Run one measure.sh at a time per
# $log_dir.
marker='// measure: one changed line'
pending=$log_dir/pending
apply() {
  cp -p "$1/$2" "$pending.orig"
  printf '%s\t%s\n' "$1" "$2" > "$pending"
  printf '%s\n' "$marker" >> "$1/$2"
}
revert() { cp "$pending.orig" "$1/$2"; rm -f "$pending" "$pending.orig"; }

# restore_pending: put back the original of a change not reverted yet.
restore_pending() {
  [ -f "$pending" ] || return 0
  IFS=$(printf '\t') read -r p_dir p_file < "$pending"
  echo "restoring $p_dir/$p_file from a change not reverted" >&2
  revert "$p_dir" "$p_file"
}

# run DIR CMDKEY CMD...: run the command in DIR (k2 in the environment of
# `cargo llvm-cov show-env` and its own target dir), stderr to $out_log; set
# wall and crates.
run() {
  dir=$1 key=$2
  shift 2
  out_log=$log_dir/last.log
  status=0
  (
    cd "$dir"
    if [ "$key" = k2 ]; then
      eval "$(cargo llvm-cov show-env --sh)"
      export CARGO_TARGET_DIR="$dir/target/llvm-cov-target"
    fi
    /usr/bin/time -p "$@"
  ) >/dev/null 2>"$out_log" || status=$?
  wall=$(awk '/^real /{w=$2} END{print w}' "$out_log")
  crates=$(grep -c '^ *Compiling ' "$out_log" || true)
}

# unchanged: drop the marker lines a change left at the end of a file the
# changes touch in A or B (a run stopped with its pending record lost), so
# every trial starts from the original.
unchanged() {
  for c in A B; do
    for ch in a b c; do
      f=$(dir_of $c)/$(file_of $c $ch)
      if [ "$(tail -n 1 "$f")" = "$marker" ]; then
        echo "dropping the measuring change left in $f" >&2
        awk -v m="$marker" '{l[NR]=$0} END{n=NR; while(n>0 && l[n]==m) n--; for(i=1;i<=n;i++) print l[i]}' "$f" > "$f.measure-tmp"
        cp "$f.measure-tmp" "$f" && rm -f "$f.measure-tmp"
      fi
    done
  done
}

trap restore_pending EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
restore_pending
unchanged

# untimed DIR CMDKEY CMD...: the same, failing the script when it fails.
untimed() {
  run "$@"
  [ "$status" = 0 ] || { echo "warm-up failed in $1: $*" >&2; cat "$out_log" >&2; exit 1; }
}

# recorded CSV ROUND ORDER: whether the trial already has its line, so a run
# stopped in the middle resumes at the first missing trial.
recorded() { [ -f "$here/$1" ] && grep -q "^$2,$3," "$here/$1"; }

csv() {
  csv_file=$here/$1
  shift
  [ -f "$csv_file" ] || echo "$header" > "$csv_file"
  echo "$*" >> "$csv_file"
}

case $section in
  build-times)
    header=round,order,condition,change,command,started_at,ended_at,wall_secs,compiling_crates
    for round in ${*:-1 2 3 4 5}; do
      order=0
      for change in a b c; do
        for key in k1 k2; do
          if [ $((round % 2)) = 1 ]; then conds="A B"; else conds="B A"; fi
          for cond in $conds; do
            order=$((order + 1))
            recorded build-times.csv $round $order && continue
            dir=$(dir_of $cond)
            file=$(file_of $cond $change)
            if [ $key = k1 ]; then cmd=$k1; else cmd=$k2; fi
            # shellcheck disable=SC2086
            untimed "$dir" $key $cmd
            apply "$dir" "$file"
            started=$(now)
            # shellcheck disable=SC2086
            run "$dir" $key $cmd
            ended=$(now)
            revert "$dir" "$file"
            cp "$out_log" "$log_dir/build-$round-$cond-$change-$key.log"
            [ "$status" = 0 ] || { echo "failed: $cond $change $key" >&2; exit 1; }
            csv build-times.csv "$round,$order,$cond,$change,$key,$started,$ended,$wall,$crates"
          done
        done
      done
    done
    ;;
  clean-builds)
    header=round,order,condition,change,command,started_at,ended_at,wall_secs,compiling_crates
    for round in ${*:-1 2 3}; do
      case $((round % 3)) in
        1) conds="A B B-dagq-only" ;;
        2) conds="B B-dagq-only A" ;;
        0) conds="B-dagq-only A B" ;;
      esac
      order=0
      for cond in $conds; do
        order=$((order + 1))
        recorded clean-builds.csv $round $order && continue
        dir=$(dir_of $cond)
        # shellcheck disable=SC2086
        untimed "$dir" k1 $k1
        case $cond in
          B) (cd "$dir" && cargo clean -p dagq -p dagq-domain) 2>/dev/null ;;
          *) (cd "$dir" && cargo clean -p dagq) 2>/dev/null ;;
        esac
        started=$(now)
        # shellcheck disable=SC2086
        run "$dir" k1 $k1
        ended=$(now)
        cp "$out_log" "$log_dir/clean-$round-$cond.log"
        [ "$status" = 0 ] || { echo "failed: clean $cond" >&2; exit 1; }
        csv clean-builds.csv "$round,$order,$cond,none,k1,$started,$ended,$wall,$crates"
      done
    done
    ;;
  domain-tests)
    header=round,order,condition,change,command,phase,started_at,ended_at,wall_secs,compiling_crates
    for round in ${*:-1 2 3}; do
      if [ $((round % 2)) = 1 ]; then conds="A B"; else conds="B A"; fi
      order=0
      for cond in $conds; do
        order=$((order + 1))
        recorded domain-tests.csv $round $order && continue
        dir=$(dir_of $cond)
        file=$(file_of $cond a)
        if [ $cond = A ]; then
          set -- --lib -E 'test(/^domain::/)'
        else
          set -- -p dagq-domain
        fi
        untimed "$dir" k1 cargo nextest run --locked --no-run "$@"
        apply "$dir" "$file"
        started=$(now)
        run "$dir" k1 cargo nextest run --locked --no-run "$@"
        ended=$(now)
        cp "$out_log" "$log_dir/tests-$round-$cond-build.log"
        [ "$status" = 0 ] || { revert "$dir" "$file"; echo "failed: tests build $cond" >&2; exit 1; }
        build_row="$round,$order,$cond,a,nextest,build,$started,$ended,$wall,$crates"
        started=$(now)
        run "$dir" k1 cargo nextest run --locked "$@"
        ended=$(now)
        revert "$dir" "$file"
        cp "$out_log" "$log_dir/tests-$round-$cond-run.log"
        # A failing test still counts its time; its exit is in the log.
        echo "round $round $cond run exit $status: $(grep -E '^ *Summary' "$out_log" | tail -1)" >> "$log_dir/tests-summary.txt"
        # Both rows of the trial go in only once its run step ended, so a
        # trial stopped between the steps has no row and is run again whole.
        csv domain-tests.csv "$build_row"
        csv domain-tests.csv "$round,$order,$cond,a,nextest,run,$started,$ended,$wall,$crates"
      done
    done
    ;;
  *) echo "unknown section: $section" >&2; exit 2 ;;
esac
