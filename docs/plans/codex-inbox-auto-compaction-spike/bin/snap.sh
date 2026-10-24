#!/bin/sh
# record mtime and sha256 of the person's codex config and auth (no copy)
for f in ~/.codex/config.toml ~/.codex/auth.json; do echo "$(date +%H:%M:%S) $1 $(basename $f) $(stat -f '%m' $f) $(shasum -a 256 $f | cut -d' ' -f1)"; done >> "$(dirname "$0")/../snap.log"
