#!/usr/bin/env bash
# Prints the ids of the smoke set, one per line, sorted: the same selection as
# `vam-e2e run --smoke` (crates/vam-e2e/src/run_cmd.rs, `select`): `needs: process`,
# not `status: retired`, no `slow` tag. Used by scripts/ci/local.sh to run smoke in
# chunks. Keep the two in step.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
awk '
function flush() { if (id != "" && n == "process" && st != "retired" && !slow) print id }
FNR == 1 { flush(); id = ""; n = ""; st = ""; slow = 0 }
/^id:/ { id = $2 }
/^needs:/ { n = $2 }
/^status:/ { st = $2 }
/^tags:/ { if ($0 ~ /[[ ,]slow[],]/) slow = 1 }
END { flush() }
' e2e/scenarios/*.yaml | sort
