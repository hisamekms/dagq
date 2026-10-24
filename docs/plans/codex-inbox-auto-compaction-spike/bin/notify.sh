#!/bin/sh
# notify gets the JSON as the last argument; keep only the head
{ printf '%s notify ' "$(date +%H:%M:%S)"; eval "printf '%s' \"\${$#}\"" | head -c 300; echo; } >> "$(dirname "$0")/../notify.log"
