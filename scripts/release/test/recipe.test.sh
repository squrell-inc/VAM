#!/usr/bin/env bash
# The base recipe check passes on the real Dockerfile and fails on refused instructions.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../../.."
check=scripts/base/check-recipe.sh
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT

bash "$check" scripts/base/Dockerfile >/dev/null || { echo "FAIL: real Dockerfile refused" >&2; exit 1; }

for bad in 'EXPOSE 80' 'CMD ["sh"]' 'ENTRYPOINT ["sh"]' 'ADD x /y' 'USER root' 'VOLUME /v' 'HEALTHCHECK NONE'; do
  printf '# syntax=docker/dockerfile:1\nFROM scratch\nRUN true\n%s\n' "$bad" > "$tmp/Dockerfile"
  if bash "$check" "$tmp/Dockerfile" >/dev/null 2>&1; then echo "FAIL: '$bad' accepted" >&2; exit 1; fi
done

# A refused word inside a continued RUN is not an instruction.
printf 'FROM scratch\nRUN echo a; \\\n    EXPOSE b\n' > "$tmp/Dockerfile"
bash "$check" "$tmp/Dockerfile" >/dev/null || { echo "FAIL: continuation line misread" >&2; exit 1; }

if bash "$check" "$tmp/none" >/dev/null 2>&1; then echo "FAIL: missing file accepted" >&2; exit 1; fi
echo "recipe.test.sh ok"
