#!/usr/bin/env bash
# Aggregate GitHub Actions step/job outcomes and fail if any is bad.
#
# Usage: gate.sh "<error message on failure>" "Label=outcome" ["Label=outcome" ...]
# Env:   GATE_STRICT=1  also treat "cancelled" and "skipped" as failures
set -euo pipefail

message=$1
shift

bad_outcomes="failure"
if [[ "${GATE_STRICT:-0}" == "1" ]]; then
    bad_outcomes="failure cancelled skipped"
fi

failed=0
for pair in "$@"; do
    label=${pair%%=*}
    outcome=${pair#*=}
    printf '%-44s %s\n' "${label}:" "${outcome}"

    for bad in $bad_outcomes; do
        if [[ "$outcome" == "$bad" ]]; then
            failed=1
        fi
    done
done

if [[ "$failed" -eq 1 ]]; then
    echo "::error::${message}"
    exit 1
fi

echo "All checks passed."
