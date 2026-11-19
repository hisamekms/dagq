#!/bin/sh
set -eu

# Classify the newline-delimited output of git diff --no-renames --name-only.
# The design subtree supplies inputs to Rust unit tests (ADR-t5487-1).
docs_only() {
    seen_path=false
    while IFS= read -r path || [ -n "$path" ]; do
        case "$path" in
            docs/design/*) return 1 ;;
            docs/*) ;;
            */*) return 1 ;;
            *.md) ;;
            *) return 1 ;;
        esac
        seen_path=true
    done
    [ "$seen_path" = true ]
}

check_case() {
    expected=$1
    label=$2
    input=$3
    actual=0
    printf '%b' "$input" | sh "$0" || actual=$?
    if [ "$actual" -ne "$expected" ]; then
        printf 'FAIL: %s (expected exit %s, got %s)\n' "$label" "$expected" "$actual" >&2
        return 1
    fi
    printf 'ok: %s (exit %s)\n' "$label" "$actual"
}

if [ "${1-}" = --self-test ]; then
    check_case 0 'development doc' 'docs/development/documents.md\n'
    check_case 0 'root Markdown' 'README.md\n'
    check_case 1 'design doc' 'docs/design/architecture.md\n'
    check_case 1 'nested design doc' 'docs/design/supervisor-lifecycle/worker-model.md\n'
    check_case 1 'plan and design docs' 'docs/plans/a.md\ndocs/design/overview.md\n'
    check_case 1 'Rust source' 'src/lib.rs\n'
    check_case 1 'plugin skill' 'plugins/claude-dagq/skills/dagq/SKILL.md\n'
    check_case 1 'empty diff' ''
else
    docs_only
fi
