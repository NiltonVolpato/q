verify: check clippy test line-count

check:
    cargo check --all-targets

clippy:
    cargo clippy --all-targets -- -D warnings

test:
    cargo test

line-count:
    #!/usr/bin/env bash
    set -euo pipefail
    offenders=$(tokei src -o json | jq -r '.Rust.reports[] | select(.stats.code >= 800) | "\(.name): \(.stats.code) code lines"')
    if [ -n "$offenders" ]; then
        echo "Files exceeding 800 code lines:"
        echo "$offenders"
        exit 1
    fi
    echo "All Rust source files are under 800 code lines."
