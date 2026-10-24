#!/bin/sh
# usage: launch.sh <cond: H|N> <model_auto_compact_token_limit>
# H: trust the 4 dagq hooks with -c hooks.state (task 3336 form); N: pass the hooks without trust
S=$(cd "$(dirname "$0")/.." && pwd -P)
cond=$1; limit=$2
state=""
set --
for pair in SessionStart:session_start:600 PreCompact:pre_compact:600 PostCompact:post_compact:600 SessionEnd:session_end:1; do
  Ev=${pair%%:*}; rest=${pair#*:}; lab=${rest%%:*}; to=${rest#*:}
  cmd="sh $S/bin/hook.sh dagq $Ev"
  set -- "$@" -c "hooks.$Ev=[{hooks=[{type=\"command\",command=\"$cmd\"}]}]"
  h=$(python3 -I "$S/bin/hash.py" "$lab" "$cmd" "$to")
  state="$state${state:+,}\"/<session-flags>/config.toml:$lab:0:0\"={trusted_hash=\"$h\"}"
done
[ "$cond" = H ] && set -- "$@" -c "hooks.state={$state}"
set -- "$@" -c "model_auto_compact_token_limit=$limit" -c "notify=[\"sh\",\"$S/bin/notify.sh\"]"
printf '%s\n' "$@" > "$S/args-$cond-$limit-$(date +%H%M%S).txt"
cd "$S/repo"
exec ~/.local/bin/codex --no-daemon \
  -c "projects={\"$S/repo\"={trust_level=\"trusted\"}}" \
  -c 'model_reasoning_effort="low"' \
  -c 'check_for_update_on_startup=false' \
  "$@"
