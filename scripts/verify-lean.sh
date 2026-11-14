#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
lean_bin="${LEAN:-lean}"
proof_output="$(mktemp)"
trap 'rm -f "$proof_output"' EXIT

# Check every standalone module. Treat admitted proofs and other warnings as
# errors, rather than accepting Lean's default successful exit after `sorry`.
for proof in proofs/*.lean; do
    printf 'Checking %s\n' "$proof"
    "$lean_bin" -j 1 -DwarningAsError=true "$proof" | tee "$proof_output"
    # Each audited theorem prints its axiom dependencies. In addition to
    # rejecting `sorry`, reject reported native-evaluation trust dependencies.
    if grep -Eq 'sorryAx|Lean[.]ofReduceBool' "$proof_output"; then
        printf 'Unaccepted axiom dependency reported in %s\n' "$proof" >&2
        exit 1
    fi
done
