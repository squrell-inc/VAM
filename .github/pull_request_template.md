## What

<!-- One change. What it does and why. Link the issue, design doc or tracker phase. -->

<!-- PRs not into main run the fast CI tier. To ask for more, put one line at the very start of a line (outside this comment), every key optional: CI: scenarios=<id>,<id> tests=workspace lane=yes release-build=yes   or   CI: full -->

## How it is proved

<!-- Scenarios turned green or added (e2e/scenarios/), tests added, docs checks. -->

- [ ] `scripts/ci/local.sh --full` passes
- [ ] Docs changed: `cargo run -p vam-e2e -- check` passes

## Checklist

- [ ] Every commit is signed off (`git commit -s`, see CONTRIBUTING.md)
- [ ] No secrets, `.env` files or real credentials
- [ ] Behaviour changes start from a scenario; no scenario's `then` was edited to pass
- [ ] Written by a coding agent? Say so here: <!-- yes / no -->
