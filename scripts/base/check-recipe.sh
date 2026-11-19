#!/usr/bin/env bash
# Checks that a base recipe stays inside the planned Vamfile grammar: only FROM, RUN, COPY,
# ENV, ARG, WORKDIR and LABEL, plus the `# syntax=` directive (design/agent-experience.md III.2).
#
#   scripts/base/check-recipe.sh [Dockerfile]     default: the Dockerfile beside this script
set -euo pipefail
file="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/Dockerfile}"
[ -f "$file" ] || { echo "check-recipe: $file not found" >&2; exit 2; }

awk -v file="$file" '
  BEGIN { cont = 0; bad = 0; seen = 0 }
  {
    line = $0
    if (cont) { cont = (line ~ /\\[ \t]*$/); next }      # continuation of the previous instruction
    sub(/^[ \t]+/, "", line)
    if (line == "") next
    if (line ~ /^#/) next                                   # comment or the syntax directive
    split(line, w, /[ \t]+/)
    ins = toupper(w[1])
    if (ins !~ /^(FROM|RUN|COPY|ENV|ARG|WORKDIR|LABEL)$/) {
      printf "%s:%d: instruction %s is outside the Vamfile grammar\n", file, NR, w[1] > "/dev/stderr"
      bad = 1
    }
    seen++
    cont = (line ~ /\\[ \t]*$/)
  }
  END {
    if (seen == 0) { print file ": no instructions" > "/dev/stderr"; exit 1 }
    exit bad
  }
' "$file"
echo "check-recipe: $file ok"
