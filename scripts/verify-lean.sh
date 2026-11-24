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
    # These standalone modules declare named theorems at top level. Keep the
    # source audit paired with the kernel's dependency report for every one.
    awk '
        $1 == "theorem" { declared[$2] = 1 }
        $1 == "#print" && $2 == "axioms" { audited[$3] = 1 }
        END {
            for (name in declared) {
                if (!(name in audited)) {
                    print "Missing #print axioms for " name > "/dev/stderr"
                    failed = 1
                }
            }
            exit failed
        }
    ' "$proof"
    "$lean_bin" -j 1 -DwarningAsError=true "$proof" | tee "$proof_output"
    # Each audited theorem prints its axiom dependencies. In addition to
    # rejecting `sorry`, reject reported native-evaluation trust dependencies.
    if grep -Eq 'sorryAx|Lean[.]ofReduceBool' "$proof_output"; then
        printf 'Unaccepted axiom dependency reported in %s\n' "$proof" >&2
        exit 1
    fi
    # Reject any newly introduced trusted axiom, not just known admissions.
    awk '
        /depends on axioms:/ {
            report = $0
            while (report !~ /\]/ && getline > 0) report = report " " $0
            sub(/^.*depends on axioms: *\[/, "", report)
            sub(/\].*$/, "", report)
            count = split(report, names, ",")
            for (i = 1; i <= count; i++) {
                gsub(/^[[:space:]]+|[[:space:]]+$/, "", names[i])
                if (names[i] != "propext" && names[i] != "Classical.choice" &&
                    names[i] != "Quot.sound") {
                    print "Unaccepted axiom: " names[i] > "/dev/stderr"
                    failed = 1
                }
            }
        }
        END { exit failed }
    ' "$proof_output"
done
