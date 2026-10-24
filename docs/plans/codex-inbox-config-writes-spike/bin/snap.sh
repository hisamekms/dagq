#!/bin/sh
# snap.sh <label>: 人の設定の更新時刻と sha256 を足す（読むだけ）
f=$HOME/.codex/config.toml
echo "$(date +%H:%M:%S) $1 config.toml mtime=$(stat -f %m "$f") sha256=$(shasum -a 256 "$f" | cut -d' ' -f1)" >> "$(dirname "$0")/../out/snap.log"
