#!/bin/sh
# usage: launch.sh <cond: A|C>   (B = A plus project .codex hooks placed separately)
S=$(cd "$(dirname "$0")/.." && pwd -P)
cond=$1
state=""
set --
for pair in SessionStart:session_start:600 SessionEnd:session_end:1 Stop:stop:600 PreToolUse:pre_tool_use:600; do
  Ev=${pair%%:*}; rest=${pair#*:}; lab=${rest%%:*}; to=${rest#*:}
  cmd="sh $S/bin/hook.sh dagq $Ev"
  set -- "$@" -c "hooks.$Ev=[{hooks=[{type=\"command\",command=\"$cmd\"}]}]"
  h=$(python3 -I "$S/bin/hash.py" "$lab" "$cmd" "$to")
  [ "$lab" = session_start ] && h=sha256:0000; state="$state${state:+,}\"/<session-flags>/config.toml:$lab:0:0\"={trusted_hash=\"$h\"}"
done
[ "$cond" = A ] && set -- "$@" -c "hooks.state={$state}"
printf '%s\n' "$@" > "$S/args-$cond-$(date +%H%M%S).txt"
cd "$S/repo"
exec ~/.local/bin/codex --no-daemon \
  -c "projects={\"$S/repo\"={trust_level=\"trusted\"}}" \
  -c 'model_reasoning_effort="low"' \
  -c 'check_for_update_on_startup=false' \
  "$@"
