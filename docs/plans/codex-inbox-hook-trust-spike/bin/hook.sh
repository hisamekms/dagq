#!/bin/sh
# $1: label (who passed it), $2: event
{ printf '%s %s %s ' "$(date +%H:%M:%S)" "$1" "$2"; head -c 400; echo; } >> "$(dirname "$0")/../hooks.log"
