# Functions that record one run of a worker's local check (ADR-t2589-1
# decision 1), sourced by scripts/cargo-brief.sh and
# scripts/check-it-test-time.sh; not run by itself. They write only when
# DAGQ_LOCAL_CHECKS_DIR names the records' directory, which dagq gives each
# of a worker's turns; without it (a person's shell, outside dagq) the
# scripts call none of them and run as before.
#
# One run leaves two files in the directory, named by the start time and a
# random part so that their names sort by start:
#   <name>.json  the record: a JSON object of command (the command line,
#                each word shell-quoted when it needs to be, at most
#                LOCAL_CHECK_COMMAND_BYTES escaped bytes, cut with "..."),
#                exit_status, started_at (UTC, RFC 3339), duration_ms
#                (milliseconds where date has %N, else whole seconds times
#                1000), kind (the check's name, or null), tree_start,
#                tree_end and log (the log's file name); the format the
#                runtime reads is on domain::local_checks::LocalCheck
#   <name>.log   the run's whole output, or its last LOCAL_CHECK_LOG_BYTES
#                bytes (default 16 MiB) after a line that says so
# The record is written to a temporary name and renamed, so a reader never
# sees half of one.
#
# A tree is the hash of the tree object of the work tree of the cwd's
# repository with every change git does not ignore (staged, unstaged and
# untracked files): git add -A into a copy of the index under ${TMPDIR}
# (with its mtime, so that git still re-reads a file changed in the second
# of the index's last write), then git write-tree. The real index is not changed, and the hash is the
# tree a commit of the whole work tree would have.
#
# A record that cannot be written (no git work tree, a directory that is
# not there or not writable) is only warned about on stderr: recording
# never changes the check's output or exit status.

: "${LOCAL_CHECK_COMMAND_BYTES:=4000}"
: "${LOCAL_CHECK_LOG_BYTES:=16777216}"

# local_check_tree prints the tree of the cwd's work tree, or fails.
local_check_tree() (
  lc_top=$(git rev-parse --show-toplevel 2>/dev/null) || exit 1
  cd "$lc_top" || exit 1
  lc_index=$(git rev-parse --git-path index 2>/dev/null) || exit 1
  case "$lc_index" in /*) ;; *) lc_index=$lc_top/$lc_index ;; esac
  lc_tmp=$(mktemp -d "${TMPDIR:-/tmp}/local-check-tree.XXXXXX") || exit 1
  trap 'rm -rf "$lc_tmp"' EXIT
  if [ -f "$lc_index" ]; then cp -p "$lc_index" "$lc_tmp/index" || exit 1; fi
  GIT_INDEX_FILE=$lc_tmp/index git add -A . >/dev/null 2>&1 || exit 1
  GIT_INDEX_FILE=$lc_tmp/index git write-tree 2>/dev/null
)

# local_check_ms prints the time in milliseconds since the epoch.
local_check_ms() {
  lc_ms=$(date +%s%N 2>/dev/null) || lc_ms=""
  case "$lc_ms" in
    ''|*[!0-9]*) echo $(( $(date +%s) * 1000 )) ;;
    *) echo $(( lc_ms / 1000000 )) ;;
  esac
}

# local_check_command prints the words "$@" as one shell command line, each
# word in single quotes when it holds more than letters, digits and
# -_./=:,+@%^.
local_check_command() {
  lc_line=""
  for lc_word in "$@"; do
    case "$lc_word" in
      ''|*[!A-Za-z0-9_./=:,+@%^-]*)
        lc_word="'$(printf '%s' "$lc_word" | sed "s/'/'\\\\''/g")'" ;;
    esac
    lc_line="${lc_line:+$lc_line }$lc_word"
  done
  printf '%s' "$lc_line"
}

# local_check_json prints $1 as a JSON string: " and \ escaped, control
# characters as \u00XX, and cut to at most LOCAL_CHECK_COMMAND_BYTES bytes
# between the quotes (a cut one ends with "..." and keeps no part of a
# multibyte character).
local_check_json() {
  LOCAL_CHECK_TEXT=$1 LC_ALL=C awk -v max="$LOCAL_CHECK_COMMAND_BYTES" '
BEGIN {
  for (i = 1; i < 256; i++) ord[sprintf("%c", i)] = i
  s = ENVIRON["LOCAL_CHECK_TEXT"]; n = length(s); out = ""; cut = 0
  for (i = 1; i <= n; i++) {
    c = substr(s, i, 1); o = ord[c]
    if (c == "\"" || c == "\\") e = "\\" c
    else if (o == 10) e = "\\n"
    else if (o == 9) e = "\\t"
    else if (o < 32) e = sprintf("\\u%04x", o)
    else e = c
    if (length(out) + length(e) > max - 3) { cut = 1; break }
    out = out e
  }
  if (cut) {
    # Drop the bytes of a multibyte character the cut split, and with them
    # the whole last one: only bytes below 128 end what is kept.
    while (length(out) > 0 && ord[substr(out, length(out), 1)] >= 128) out = substr(out, 1, length(out) - 1)
    out = out "..."
  }
  printf "\"%s\"", out
}'
}

# local_check_begin COMMAND... notes the start of a run: its command line,
# time and tree. It sets local_check_started (0 when it cannot record).
local_check_begin() {
  local_check_started=0
  [ -n "${DAGQ_LOCAL_CHECKS_DIR:-}" ] || return 0
  local_check_cmd=$(local_check_command "$@")
  local_check_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  local_check_stamp=$(date -u +%Y%m%dT%H%M%SZ)
  local_check_start_ms=$(local_check_ms)
  if ! local_check_tree_start=$(local_check_tree); then
    echo "${me:-local-check}: no record of this run: cannot read the tree of the work tree" >&2
    return 0
  fi
  local_check_started=1
}

# local_check_end STATUS KIND LOG writes the record of the run that
# local_check_begin started, with its exit status STATUS, its kind KIND
# (empty for none) and the whole output in the file LOG.
local_check_end() {
  [ "${local_check_started:-0}" = 1 ] || return 0
  local_check_started=0
  lc_end_ms=$(local_check_ms)
  if ! lc_tree_end=$(local_check_tree); then
    echo "${me:-local-check}: no record of this run: cannot read the tree of the work tree" >&2
    return 0
  fi
  lc_dir=$DAGQ_LOCAL_CHECKS_DIR
  if ! lc_name=$(mktemp "${lc_dir%/}/$local_check_stamp-XXXXXX" 2>/dev/null); then
    echo "${me:-local-check}: no record of this run: cannot write in $lc_dir" >&2
    return 0
  fi
  lc_bytes=$(wc -c < "$3" 2>/dev/null | tr -d ' ') || lc_bytes=0
  if [ "${lc_bytes:-0}" -gt "$LOCAL_CHECK_LOG_BYTES" ]; then
    { echo "${me:-local-check}: the first $(( lc_bytes - LOCAL_CHECK_LOG_BYTES )) byte(s) of the output are left out"
      tail -c "$LOCAL_CHECK_LOG_BYTES" "$3"; } > "$lc_name.log"
  else
    cp "$3" "$lc_name.log"
  fi
  if [ -n "$2" ]; then lc_kind="\"$2\""; else lc_kind=null; fi
  lc_duration=$(( lc_end_ms - local_check_start_ms ))
  [ "$lc_duration" -ge 0 ] || lc_duration=0
  printf '{"command":%s,"exit_status":%d,"started_at":"%s","duration_ms":%d,"kind":%s,"tree_start":"%s","tree_end":"%s","log":"%s"}\n' \
    "$(local_check_json "$local_check_cmd")" "$1" "$local_check_at" "$lc_duration" "$lc_kind" \
    "$local_check_tree_start" "$lc_tree_end" "$(basename "$lc_name").log" > "$lc_name.tmp" &&
    mv "$lc_name.tmp" "$lc_name.json" ||
    echo "${me:-local-check}: no record of this run: cannot write $lc_name.json" >&2
  rm -f "$lc_name" "$lc_name.tmp"
}
