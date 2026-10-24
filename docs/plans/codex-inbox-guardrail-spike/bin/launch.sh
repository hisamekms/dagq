#!/bin/sh
# usage: launch.sh <tag>  -- one PreToolUse hook, trusted by -c hooks.state (task 3336 form)
S=$(cd "$(dirname "$0")/.." && pwd -P)
cmd="python3 -I $S/bin/guard.py"
h=$(python3 -I "$S/bin/hash.py" pre_tool_use "$cmd" 600)
set -- -c "hooks.PreToolUse=[{hooks=[{type=\"command\",command=\"$cmd\"}]}]" \
       -c "hooks.state={\"/<session-flags>/config.toml:pre_tool_use:0:0\"={trusted_hash=\"$h\"}}"
printf '%s\n' "$@" > "$S/args/args-$1-$(date +%H%M%S).txt"
cd "$S/repo"
PATH="$S/stubbin:$PATH" exec ~/.local/bin/codex --no-daemon \
  -c "projects={\"$S/repo\"={trust_level=\"trusted\"}}" \
  -c 'model_reasoning_effort="low"' \
  -c 'check_for_update_on_startup=false' \
  "$@"
