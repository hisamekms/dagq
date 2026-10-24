#!/bin/sh
# run.sh <回> <E|M> [-c の引数 ...]: 新しい CODEX_HOME で TUI を起動し、前後の config.toml を残す
set -u
S=$(cd "$(dirname "$0")/.." && pwd -P)
n=$1; init=$2; shift 2
H=$S/home-$n; O=$S/out/run$n; mkdir -p "$H" "$O"
if [ "$init" = M ]; then
  printf '[projects."%s"]\ntrust_level = "trusted"\n' "$S/repo" > "$H/config.toml"
fi
[ -f "$H/config.toml" ] && cp "$H/config.toml" "$O/config.before.toml"
echo "before mtime=$(stat -f %m "$H/config.toml" 2>/dev/null || echo none)" > "$O/mtime.txt"
(cd "$H" && find . -type f | sort) > "$O/files.before.txt"
"$S/bin/snap.sh" "run$n-before"
T="tmux -L dagq-t3338 -f /dev/null"
printf '%s\n' "$@" > "$O/extra-args.txt"
date +%H:%M:%S > "$O/start.txt"
$T new-session -d -s r$n -x 200 -y 50 -c "$S/repo" env CODEX_HOME="$H" ~/.local/bin/codex --no-daemon \
  -c "projects={\"$S/repo\"={trust_level=\"trusted\"}}" \
  -c 'check_for_update_on_startup=false' "$@"
sleep 10
$T capture-pane -p -t r$n > "$O/screen.txt"
$T send-keys -t r$n C-c; sleep 1; $T send-keys -t r$n C-c; sleep 2
$T kill-session -t r$n 2>/dev/null
echo "after mtime=$(stat -f %m "$H/config.toml" 2>/dev/null || echo none)" >> "$O/mtime.txt"
[ -f "$H/config.toml" ] && cp "$H/config.toml" "$O/config.after.toml"
(cd "$H" && find . -type f | sort) > "$O/files.after.txt"
"$S/bin/snap.sh" "run$n-after"
b="$O/config.before.toml"; [ -f "$b" ] || b=/dev/null
a="$O/config.after.toml"; [ -f "$a" ] || a=/dev/null
diff -u "$b" "$a" > "$O/config.diff"
echo "== run $n ($init) $*"; cat "$O/mtime.txt"; echo "-- diff"; cat "$O/config.diff"; echo "-- screen"; grep -v '^\s*$' "$O/screen.txt" | head -15
