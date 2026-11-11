# E2E scenarios

Runnable behaviour specs. Design: `design/scenarios.md`. A phase is planned as
the scenarios it adds, changes or retires; they are written first as `pending`.

## Run

```
scripts/e2e/run.sh --smoke                 # the smoke set
scripts/e2e/run.sh --scenarios a,b         # named scenarios
scripts/e2e/run.sh --tags chat | --all     # by area, or everything
```

One line per scenario, `summary.json` and `briefs/<id>.md` per failure in the
`--out` directory (default `target/e2e/<time>`). Exit 0 or 1. It builds a
test-only `vamd` and `vam-runtime` (`process-guest`; guests are host processes,
mock model only) and runs on any Linux box, no KVM. CI runs `--smoke` on every PR.

### The microVM lane

```
scripts/e2e/run.sh --firecracker           # every non-retired `needs: firecracker` scenario
```

Builds the default `vamd` (no `process-guest`), starts it with real Firecracker and snapshot
restore on (the node bakes the golden snapshot before the API answers, inside a jail on a jailed
node, the default), and runs the
scenarios with the same mock model. The node runs the jailer, which needs **root**: build as your
user with `VAM_E2E_BUILD_ONLY=1 scripts/e2e/run.sh --firecracker`, then run
`sudo env "PATH=$PATH" <the asset variables> VAM_E2E_SKIP_BUILD=1
VAM_E2E_VAMD=$PWD/target/e2e-bin-vm/vamd scripts/e2e/run.sh --firecracker` (`sudo -E` is ignored on hosted runners) (an unprivileged run stops with that hint, class `env`).
Needs `/dev/kvm` and the assets
`scripts/e2e/firecracker-assets.sh --env-file FILE` exports (`VAMD_FIRECRACKER_BIN`,
`VAMD_JAILER_BIN`, `VAM_KERNEL_PATH`, `VAM_ROOTFS_PATH`). A node that does not report
`guestMode: vm` fails each scenario at `when.0`, class `env`. Briefs include Firecracker's
stdout and stderr (`/tmp/vam-fc-*`). CI runs it in the advisory `firecracker` job; selection is by
`needs`, so a new firecracker scenario joins the lane without a CI change. Not combinable with
`--smoke`.

A pull request that does not need the paid lane carries the `skip-vm-lane` label: the `changes`
job then sets `vm=false` and the `firecracker` job does not start. Push and `workflow_dispatch`
runs ignore the label. The label is read when the run starts, so open such a PR as a draft (a draft
runs no CI), add the label, then mark it ready for review.

### Real-model benchmark runs (microVM lane)

`--bench --real --model <catalog model>` (or `--provider vercel --model <gateway id>`, below) runs only on the microVM lane (`--firecracker`). A
process-mode node serves the mock model only (decision S5), and a run on one is refused. The
provider key never goes through the runner: it lives in the vault of a **bench node** you start on
a persistent state dir, and the runner only asks that node whether it can serve the model.

Gates, in order (each refuses with exit 2 before anything is spent): the model is in the catalog;
`--real` is given; `--firecracker` is given; the node reports `guestMode: vm` (`GET /v1/node`);
the node's org secret store holds the catalog's `secret_ref` for the model, `anthropic/api-key`
for the Claude models (`GET /v1/secrets?scope=org`, metadata only: the runner never reads a
value). A missing credential prints the `vam secrets set` command that adds it.

`--state-dir DIR` (on `run` and `node`) keeps the node's state, including its vault, in `DIR`.
`VAMD_SECRETS_MASTER_KEY` **must** come from your environment (64 hex characters, e.g.
`openssl rand -hex 32`) and must not be the built-in test constant that throwaway nodes use;
the runner refuses to start otherwise. Keep that key in your Keychain, never in a file in the
repository. Lose it and the vault cannot be opened: add the provider key again to a new state dir.
The node's own admin key is made once in `DIR/node-key` (mode 0600). Throwaway nodes (smoke, CI,
`--bench --mock`) are unchanged.

**Runbook (the Mac, with the microVM lane inside the Lima VM).** Nothing below spends money
until step 8. Every command runs in the Mac's terminal, from the repository checkout (Lima mounts
it at the same path, read-only). Fixes from the first real run (2026-10-27, run tag `d40996`):
the rootfs and every `--out` live on Lima's own disk, because the jailer hard-links the rootfs
into the jail and cannot across filesystems; steps 5 to 9 are single Mac lines that pass the
master key into Lima explicitly; a provider key already in a Mac variable is passed into Lima by
name.

1. Once: `limactl start --name=vam scripts/lima-macos.yaml` (skip if the instance exists),
   `./scripts/build-rootfs-lima.sh` (writes `/tmp/lima/rootfs.ext4`), `./scripts/start-vamd-lima.sh`
   once so it installs Firecracker, the jailer and the guest kernel inside Lima, then
   `limactl shell vam -- pkill vamd` (that dev node is not used here). Copy the rootfs to Lima's
   own disk: `limactl shell vam -- sudo install -D -m 0644 /tmp/lima/rootfs.ext4 /var/lib/vam/rootfs.ext4`.
2. Once: put a master key in the Keychain:
   `security add-generic-password -a "$USER" -s vam-bench-master-key -w "$(openssl rand -hex 32)"`.
3. In each Mac terminal you use below, define the helper (it reads the master key from the
   Keychain at each call and runs the command as root inside Lima with the lane's environment):
   ```
   vamlima() { limactl shell vam -- sudo env \
     "VAMD_SECRETS_MASTER_KEY=$(security find-generic-password -s vam-bench-master-key -w)" \
     CARGO_TARGET_DIR=/tmp/vam-target VAM_E2E_SKIP_BUILD=1 VAM_E2E_VAMD=/tmp/vam-target/e2e-bin-vm/vamd \
     VAMD_FIRECRACKER_BIN=/usr/local/bin/firecracker VAMD_JAILER_BIN=/usr/local/bin/jailer \
     VAM_KERNEL_PATH=/var/lib/vam/kernels/vmlinux-6.18 VAM_ROOTFS_PATH=/var/lib/vam/rootfs.ext4 \
     "PATH=/tmp/vam-target/debug:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" "$@"; }
   TASKS=tool-call-first-try-saves-a-draft,matching-request-starts-the-playbook,greeting-starts-no-playbook,injected-text-cannot-start-a-playbook
   ```
   The master key is on the command line of that one `sudo env` call while it runs; on a
   single-user Mac that is acceptable, on a shared machine use the two-shell form from git history.
4. Build inside Lima as your user (not root):
   ```
   limactl shell vam -- bash -lc 'export CARGO_TARGET_DIR=/tmp/vam-target VAMD_FIRECRACKER_BIN=/usr/local/bin/firecracker VAMD_JAILER_BIN=/usr/local/bin/jailer VAM_KERNEL_PATH=/var/lib/vam/kernels/vmlinux-6.18 VAM_ROOTFS_PATH=/var/lib/vam/rootfs.ext4 && VAM_E2E_BUILD_ONLY=1 scripts/e2e/run.sh --firecracker && cargo build -p vam-cli'
   ```
5. Terminal A, start the bench node on Lima's disk and leave it running:
   `vamlima /tmp/vam-target/debug/vam-e2e node --firecracker --state-dir /root/vam-bench-node`.
   It prints `node up: https://127.0.0.1:<port>`.
6. Terminal B, once per state dir: add the provider key to the node's vault. Through Vercel AI
   Gateway (the owner's route), with the key typed at a hidden prompt:
   ```
   vamlima bash -c 'read -rsp "gateway key: " K; echo; VERCEL_AI_GATEWAY_KEY="$K" VAM_CA_FILE=/root/vam-bench-node/tls/ca.pem vam secrets set vercel/api-key --from-env VERCEL_AI_GATEWAY_KEY --scope org --host https://127.0.0.1:<port> --token "$(cat /root/vam-bench-node/node-key)"'
   ```
   If the key is already in a Mac variable, pass it into Lima by name instead (it is then on that
   one command line while it runs):
   ```
   vamlima "VERCEL_AI_GATEWAY_KEY=$VERCEL_AI_GATEWAY_KEY" bash -c 'VAM_CA_FILE=/root/vam-bench-node/tls/ca.pem vam secrets set vercel/api-key --from-env VERCEL_AI_GATEWAY_KEY --scope org --host https://127.0.0.1:<port> --token "$(cat /root/vam-bench-node/node-key)"'
   ```
   Anthropic direct is the same with `anthropic/api-key` and `ANTHROPIC_API_KEY`. Then press Enter
   in terminal A to stop the node. The key stays in `/root/vam-bench-node/vamd.db`, encrypted
   under your master key.
7. Free proof of the lane (mock bench), then copy the result where the Mac can read it:
   ```
   vamlima scripts/e2e/run.sh --firecracker --bench --mock --scenarios "$TASKS" -k 1 --out /root/bench-mock && limactl shell vam -- sudo cp -r /root/bench-mock /tmp/lima/
   ```
8. **Smoke, k = 1, Haiku only (4 runs, a few cents).** Confirm the gateway ids on Vercel's model
   list first (`anthropic/claude-haiku-5.5`, `anthropic/claude-sonnet-5.5` as of 2026-10-27):
   ```
   vamlima scripts/e2e/run.sh --firecracker --bench --real --provider vercel --model anthropic/claude-haiku-5.5 --scenarios "$TASKS" -k 1 --state-dir /root/vam-bench-node --budget 20m --out /root/bench-smoke && limactl shell vam -- sudo cp -r /root/bench-smoke /tmp/lima/
   ```
   Anthropic direct: drop `--provider vercel` and use `--model claude-haiku-5-5`.
   Check `/tmp/lima/bench-smoke/bench-results.json`: four trials, `mock: false`, `endpoint`
   `ai-gateway.vercel.sh` and `provider: vercel` (direct: `api.anthropic.com`), a cost on every
   row at the maker's list price (`priceSource`; `gatewayCostPerSuccess` is internal and null while
   the gateway's own charge is not captured), no key anywhere
   (`grep -rE 'sk-|vck_' /tmp/lima/bench-smoke` finds nothing). A trial that fails its checks is a
   measurement, not an error. A "results:" write error for `bench/results` is expected (the
   repository is read-only in Lima); the file in `--out` is the result.
9. **Baseline, both models, k = 5 (40 runs, on the owner's word):**
   ```
   vamlima scripts/e2e/run.sh --firecracker --bench --real --provider vercel --model anthropic/claude-sonnet-5.5,anthropic/claude-haiku-5.5 --scenarios "$TASKS" -k 5 --state-dir /root/vam-bench-node --budget 90m --out /root/bench-baseline && limactl shell vam -- sudo cp -r /root/bench-baseline /tmp/lima/
   ```
   The first baseline (run tag `d40996`) cost 0.16 USD at Anthropic's list price.
    Under `--bench --real` the step budget defaults to 180 s and the scenario budget (per trial) to 10 min; `--step-budget <dur>` and `--scenario-budget <dur>` override them on any run (mock and smoke keep 30 s / 120 s). `bench-results.json` records both, and a row's `budgetTimeouts` counts trials cut off by a budget (still failures).

10. On the Mac: copy `/tmp/lima/bench-baseline/bench-results.json` to
    `bench/results/<task-set version>.json` and commit it with the table on the results page. If
    a run was interrupted: `limactl shell vam -- sudo pkill firecracker`.

### Bench on GitHub Actions

`.github/workflows/bench.yml` runs the real-model benchmark on a hosted KVM runner, so the Mac is not needed. It is
`workflow_dispatch` only (no push or pull request trigger), so the secret never reaches a fork PR. Two benches never
overlap (`concurrency: bench`, no cancel).

- **Secret.** The repository secret `VERCEL_AI_GATEWAY_API_KEY` (Settings, Secrets and variables, Actions). In a real
  run the job starts a throwaway bench node with a fresh master key, binds the key into its vault as `vercel/api-key`
  (`vam secrets set --from-env`), stops the node and runs the bench on that state dir. The key is in one step's
  environment only, is never written to an uploaded file, and a key grep (`sk-` or `vck_` plus 10 key characters) over the
  output fails the job on a match. A missing secret fails the job early with a message; `mock: true` needs none.
- **Dispatch.** Actions tab, "bench", Run workflow, or:
  ```
  gh workflow run bench.yml --ref main -f mock=true -f k=1                      # free dry run
  gh workflow run bench.yml --ref main -f models=anthropic/claude-haiku-5.5 -f k=1 -f max_usd_per_job=0.2 \
    -f scenarios=tool-call-first-try-saves-a-draft                              # one real trial
  gh workflow run bench.yml --ref main -f models=anthropic/claude-sonnet-5.5,zai/glm-5 -f profiles=base -f k=5 -f max_usd_per_job=2
  ```
- **Kill-mid-task row (task set `0.3.0`).** `u5-kill-mid-task` is the scenario `task-killed-mid-effect-finishes-once` (process lane) and its microVM copy `vm-task-killed-mid-effect-finishes-once` (`kill-guest` kills the session's Firecracker; needs root and the test broker `run.sh --firecracker` exports): the
  guest is killed while a ticket create is in flight and the task must finish with one create in the test broker's
  ledger. Its row (and every row) carries `duplicateEffects` (creates in the world's ledger beyond one per intended
  effect) and `lostTasks` (a run that never finished, or ended on a `guest_crashed`, `vamd_restarted`, `crashed_repeatedly`,
  `restarted_repeatedly` or `not_resumable` reason; an expired queued fire is not lost). Both must be 0, or the trial is red. Free proof on the mock:
  `scripts/e2e/run.sh --bench --mock --task-set-version 0.3.0 --scenarios task-killed-mid-effect-finishes-once -k 1`.
  Real-model row (on the owner's word, not dispatched by the wave):
  ```
  gh workflow run bench.yml --ref main -f task_set=0.3.0 -f scenarios=vm-task-killed-mid-effect-finishes-once \
    -f models=anthropic/claude-sonnet-5.5,anthropic/claude-haiku-5.5 -f profiles=base -f k=5 -f max_usd_per_job=2
  ```
- **Inputs.** `ref` (default the dispatched ref), `task_set` (default `0.1.0`), `models` (comma list of catalog ids),
  `profiles` (comma list, default `base`), `mode` (`on`; `off` and `both` are refused until the runner has a
  machine-off flag), `k` (default 5), `max_usd_per_job` (default 2), `scenarios` (comma list of scenario ids; default
  the four 0.1.0 bench scenarios), `mock` (default false).
- **Jobs.** One job per model and profile, one at a time (`max-parallel: 1`), 300 minutes each, so one model's rate
  limits or a runner failure does not take the others down. `--max-usd` applies per job: past the cap no further trial
  starts, the job ends with a warning, the result is partial (`stoppedAt: "max-usd"`) and `bench-merge` refuses it.
- **Artifacts.** `bench-<task_set>-<model>-<profile>-<run_id>` holds `bench-results.json`, `summary.json`, `briefs/` and
  the vamd log. The job summary has one row per task (model, mode, profile, successes/k, cost per success).
- **Merge.** On a machine with a writable checkout:
  ```
  gh run download <run_id> -D /tmp/bench-<run_id>
  for f in /tmp/bench-<run_id>/*/bench/bench-results.json; do cargo run -q -p vam-e2e -- bench-merge "$f"; done
  grep -rE '\bsk-[A-Za-z0-9_-]{10,}|vck_[A-Za-z0-9_-]{10,}' /tmp/bench-<run_id> bench   # finds nothing
  ```
- **Run id on each published row.** The runner draws its run tag at random and has no flag to set it, so a row's
  `runTag` is not the Actions run id. Record the run id in the results report next to the artifact name. Follow-up: a
  `--run-tag` flag on `vam-e2e run`, added in B1's first wave, lets the workflow pass `github.run_id`.
- **Fallback.** The Mac runbook above stays valid, for any run the hosted runner cannot do.

#### Open models and harness profiles (H-1.8)

`--profile <name>` (with `--bench`) sends `inputs.harness.profile` on every trial's chat. Without it no profile is
sent and the node uses `base`, as before. A name the node does not know (`base`, `open-strong`, `open-small`) is
refused with exit 2 before any trial. `open-strong` and `open-small` have pending evidence, so they run only when named.

`--max-usd <amount>` (with `--bench`) caps the run's spend, summed after each trial from the same cost the rows
publish (the maker's list price). When the sum passes the cap the runner starts no further trial (a running trial is
not killed), writes `bench-results.json` with `stoppedAt: "max-usd"`, `maxUsd` and `spendMicro`, and exits 3. A trial
whose cost is unknown stops the run too (`stoppedAt: "cost-missing"`). A stopped result is never copied to `bench/results`.
Under `--mock` the cap counts 1 micro-USD per token, so it can be tried for free.

Rows carry `harnessProfile`, `harnessVersion`, `harnessDigest`, `harnessOverrides` and `runTag`. A publishable run is
merged into `bench/results/<version>.json` by key (task, model, mode, profile): other rows stay byte-identical, the same key
replaces only its row, and the file becomes schema 2 with a `runs` list (a schema 1 file is read as one run). The
per-run top-level fields read `mixed` once rows from several runs coexist; read the rows and `runs`. Run these from a
checkout where `bench/results` is writable (Lima mounts the repository read-only, so merge on the Mac, step 13). The
55cb5b Claude rows are never rewritten.

Total budget for this section: 10 USD. `$TASKS` and `vamlima` are from step 3.

11. Smoke, k = 1, per model (4 runs each, cents). It confirms the gateway ids on Vercel's model list
    (`zai/glm-5`, `alibaba/qwen3.6-27b`) and the adapter wire fragments: thinking off for Qwen, strip for GLM. Check each
    `bench-results.json`: `mock: false`, a cost on every row, `harnessProfile` as named, no 400 naming the adapter.
    ```
    vamlima scripts/e2e/run.sh --firecracker --bench --real --provider vercel --model zai/glm-5 --profile open-strong --scenarios "$TASKS" -k 1 --max-usd 0.25 --state-dir /root/vam-bench-node --out /root/bench-glm-smoke
    vamlima scripts/e2e/run.sh --firecracker --bench --real --provider vercel --model alibaba/qwen3.6-27b --profile open-small --scenarios "$TASKS" -k 1 --max-usd 0.25 --state-dir /root/vam-bench-node --out /root/bench-qwen-smoke
    ```
12. Paid runs (Q9), k = 5, task set 0.1.0, one run per model per profile, each with its own `--out` and `--max-usd`.
    GLM is capped at 2 USD in all (1 + 1) and Qwen at 1 USD in all (0.5 + 0.5), so the four runs stay under the 10 USD total:
    ```
    vamlima scripts/e2e/run.sh --firecracker --bench --real --provider vercel --model zai/glm-5 --profile base --task-set-version 0.1.0 --scenarios "$TASKS" -k 5 --max-usd 1 --state-dir /root/vam-bench-node --out /root/bench-glm-base
    vamlima scripts/e2e/run.sh --firecracker --bench --real --provider vercel --model zai/glm-5 --profile open-strong --task-set-version 0.1.0 --scenarios "$TASKS" -k 5 --max-usd 1 --state-dir /root/vam-bench-node --out /root/bench-glm-open-strong
    vamlima scripts/e2e/run.sh --firecracker --bench --real --provider vercel --model alibaba/qwen3.6-27b --profile base --task-set-version 0.1.0 --scenarios "$TASKS" -k 5 --max-usd 0.5 --state-dir /root/vam-bench-node --out /root/bench-qwen-base
    vamlima scripts/e2e/run.sh --firecracker --bench --real --provider vercel --model alibaba/qwen3.6-27b --profile open-small --task-set-version 0.1.0 --scenarios "$TASKS" -k 5 --max-usd 0.5 --state-dir /root/vam-bench-node --out /root/bench-qwen-open-small
    ```
    A run that exits 3 hit its cap: its file is partial and was not published. Raise the cap only on the owner's word.
13. Merge on the Mac, in a writable checkout, one command per run directory copied out of Lima
    (`/tmp/lima/bench-glm-base`, `bench-glm-open-strong`, `bench-qwen-base`, `bench-qwen-open-small`):
    ```
    for d in /tmp/lima/bench-glm-base /tmp/lima/bench-glm-open-strong /tmp/lima/bench-qwen-base /tmp/lima/bench-qwen-open-small; do
      cargo run -q -p vam-e2e -- bench-merge "$d/bench-results.json"
    done
    ```
    It writes `bench/results/<taskSetVersion>.json` (or `--into <file>`) and prints each row it added or replaced. It
    refuses a mock, recorded, synthetic or cost-capped (`stoppedAt`) result. `git diff bench/results/0.1.0.json` must
    show no change inside the 55cb5b rows.
14. Key grep before any commit, as in `bench/results/0.1.0-report.md`:
    `grep -rE 'sk-|vck_' /tmp/lima/bench-* bench docs e2e` finds nothing.
15. DeepSeek (`deepseek/deepseek-v4-pro`, priced at DeepSeek's peak list price). Smoke first, k = 1. It is also the first
    live check of the `keep` thinking mode: no 400 on the tool rounds. Check `bench-results.json` as in step 11.
    ```
    vamlima scripts/e2e/run.sh --firecracker --bench --real --provider vercel --model deepseek/deepseek-v4-pro --profile open-strong --scenarios "$TASKS" -k 1 --max-usd 0.25 --state-dir /root/vam-bench-node --out /root/bench-deepseek-smoke
    ```
    Then the paid runs, k = 5, task set 0.1.0, each capped at 1 USD:
    ```
    vamlima scripts/e2e/run.sh --firecracker --bench --real --provider vercel --model deepseek/deepseek-v4-pro --profile base --task-set-version 0.1.0 --scenarios "$TASKS" -k 5 --max-usd 1 --state-dir /root/vam-bench-node --out /root/bench-deepseek-base
    vamlima scripts/e2e/run.sh --firecracker --bench --real --provider vercel --model deepseek/deepseek-v4-pro --profile open-strong --task-set-version 0.1.0 --scenarios "$TASKS" -k 5 --max-usd 1 --state-dir /root/vam-bench-node --out /root/bench-deepseek-open-strong
    ```


`cargo run -p vam-e2e -- check` lints the catalog without running anything. It also lints the docs
(`--docs`, default `docs/src/content/docs`, and `--readme`, default `<root>/../README.md`; a default
that is absent is skipped, an explicit path that is absent exits 2): every page needs `status:
shipped | planned | stale`; a `shipped` page whose prose matches a word in `e2e/docs-claims.yaml`
needs `proves:` (scenario ids, which must be `active`, or test paths `crates/x/tests/y.rs` or
`...::fn_name`); hand-written `:::note[Status` blocks are errors. `e2e/docs-baseline.txt` lists the
pages that fail today: a listed page is exempt, a listed page that passes is an error, so the list
only shrinks. The README keeps its keys in a leading `<!-- docs: ... -->` comment.

Two more files feed the same `check`:

- `e2e/docs-features.yaml` is the feature register: one row per shipped feature with `dev` and
  `arch` page lists (slugs under `docs/src/content/docs`), and `proves` (active scenario ids or
  test paths) or `unproven: <reason>`. Rule `docs-feature`: every listed page exists (or the row
  says `pending_page: true` until its wave writes it); a page that has left the baseline must be
  `shipped` and name at least one of the row's proofs; every proof resolves. A test proof is a
  file under a crate's `tests/` directory, or `path::fn` where `fn` has a `#[test]` or
  `#[tokio::test]` attribute: a bare `src/` file is not a proof. An `unproven` reason says why no
  test proves the feature and never names a proof. `check` prints
  `features: N, proven M, with a developer page K`. A feature that a lane adds gets its row when
  that lane reaches the base.
- `docs/redirects.json` maps an old URL to `{ "to": "/new/", "kind": "retired" | "design" }`.
  `docs/astro.config.mjs` reads it into Astro's `redirects`; rule `docs-redirect` checks that the
  old page is gone, the target is a page (not itself redirected) and no page links to an old URL
  (absolute links only). Pages moved to `design/docs-archive/` use kind `design`.

Style rules for docs pages:

- Plain words, short sentences, the reader's task first. No marketing words.
- Every command shown was run against a process-mode node and its output pasted as it printed
  (secrets and ids shortened with `…`). A step only a microVM can show cites its lane scenario.
- Every security sentence names its proof (scenario or test) or says it is not proved.
- Nothing about unbuilt features except on a page or section marked `planned`; no phase numbers
  or branch names on public pages.

## Write one

A scenario is `e2e/scenarios/<name>.yaml`; world pieces it arranges state from
are in `e2e/world/`. Copy `agent-workspaces-are-isolated.yaml`.

- `id` equals the file name: lowercase words describing the behaviour. No plan
  numbers (epic, phase, wave, story).
- `given` lists world pieces (`as:` renames); `when` and `then` are steps;
  `guards` are the files whose change should run it; `mock` is the scripted
  model, matched by the `e2e-step:<label>` in the user text.
- Steps beyond `chat`, `expect`, `approve`, `deny`: `chat { text, thread: continue }`
  continues a thread; `chat { wait: false }` returns once the run is created (to
  stop a task that is still running); `stop-session: {}` terminates the latest
  run's session and waits for the run to end; `restart-vamd: {}` restarts vamd
  and waits for health (durable state must survive).
- Steps that talk to a task while it works: `send { text, mode: steer|queue,
  refused, as }` posts a message on the latest run's thread (`POST /v1/runs`
  with `inboxMode`, one post, no busy retry); a `queue` message becomes a run
  prompt, so it carries the mock marker, and the step follows the run it
  starts (it becomes the latest run). `stop { refused }` calls
  `POST /v1/runs/{id}/stop` on the latest run and waits until it is terminal.
  `answer { response, refused, as }` decides the latest pending question
  (`APPROVAL_KIND_QUESTION`) with `HITL_DECISION_SUBMITTED` and the response.
  With `refused: true` a step passes only on a 4xx (and, for `answer`, when
  the question stays pending). `decide-together { option, answer, as }` waits
  until the latest run has both a pending gate and a pending question, then
  decides the gate with `option` and answers the question with `answer` in two
  requests sent in parallel (two decisions racing to resume one parked run). Events `inbox.received`, `inbox.delivered`,
  `inbox.discarded`, `inbox.started`, `outbox.message` and `outbox.card` may
  be expected.
- `expect: { thread: { role, contains } }` reads the latest run's thread through
  `GET /v1/workspaces/{ws}/threads/{thread}/messages` (`{records:[{seq, kind,
  message:{role, parts[]}}], integrity, source}`) and needs a message of that role
  whose text, tool-call arguments or tool-result text contains the string.
- A chat text may carry several `e2e-step:` markers (a replayed history); the
  mock uses the last one. Mock rule `anyContains` matches any earlier
  non-system message, so a follow-up can depend on an earlier tool result.
- Assert what a user sees (CLI, API, events), not internals.
- New scenarios land `status: pending` and must fail at the step the phase adds
  (class `product`). Flip to `active` when the code makes them pass.
- Never edit a `then` to make a scenario pass: that is a spec change, listed in
  the PR and approved by the orchestrator or owner.
- Release builds never contain process mode: `scripts/e2e/check-release-gate.sh`.

## Steps for approvals, drafts and a second identity

- `chat` and `start-playbook` take `draft: true` (the owner's draft of the agent),
  `draft: false` (pin the release, so a draft saved by "Always" is not followed) and
  `as: <org alias>` (run as that identity). Without `draft`, a run follows the
  owner's draft once one exists. A draft run has its own workspace (vamd binds a
  workspace to one agent version).
- `approve` / `deny` take `latest` or `{ always: true, refused: true, as: <org alias> }`.
  `always` also saves the decision on the agent's draft (Always allow / Always
  deny). `refused: true` passes only on a 4xx answer with the approval still A reject that is not remembered sends a fixed `rejectionReason`.
  pending. `decide-gate` takes `refused` and `as` the same way.
- `expect: { approval: pending, tool, fields }` and
  `expect: { gate: pending, step, fields }` match `fields` on the pending claims
  (exact dotted camelCase paths, `{ contains: text }` for a substring).
- An `org` piece with a role other than `Owner` creates a user and one key for
  the scenario. Its runs, approvals and decisions use that key and its own
  workspace. The Owner is the bootstrap key.
- `expect: { event: <type>, tool, fields, absent: true }` passes when no such event appeared by
  the time the run ended or parked and settled, and fails as soon as one appears.
- `expect: { run: <state> }` on a run parked on a pending approval or gate fails
  after the settle time as `product` ("parked on approval for <tool>").

## Receipt expectations and offline verify

- `expect: { receipt: { fields } }` reads `GET /v1/runs/{id}/receipt` of the latest run and
  matches `fields` as dotted camelCase paths over `{receipt, modelAppendix, signature}` (array
  items by index, e.g. `receipt.toolCalls.lines.0.tool`). A non-2xx answer is `product`, even when
  every field is `absent`.
- Any `fields` family (event, approval, gate, node, receipt) also takes `{ present: true }` and
  `{ absent: true }` (a missing path or `null` is absent; `false` flips the meaning).
- `verify-receipt: {}` writes the latest run's receipt under a scratch directory and runs the
  built `vam receipt verify <file>` (path in `VAM_E2E_CLI`, exported by `run.sh`) with no host and
  no key; it passes on exit 0. `verify-receipt: { tamper: true }` changes one byte inside the
  signed `receipt` object and passes only on a non-zero exit whose output names the
  signature. `verify-receipt: { pack: true }` writes the run's evidence pack
  (`GET /v1/runs/{id}/evidence`, which must hold `receipt.json`) and runs `vam evidence verify
  <dir>`. An unset or missing `VAM_E2E_CLI` is `env`; a wrong exit is `product`.
- `cli: { args, root, exit, contains, absent, absentInFiles, unchanged, capture }` runs the
  built `vam` on a scratch copy of a fixture directory (`root: { from: <dir under world/fixtures> }`,
  or `root: { same: true }` for the tree the previous `cli` step left), with an empty environment:
  no host, no key. `{{root}}` (the copy) and `{{vamd}}` (`VAM_E2E_VAMD`) are replaced in `args`;
  `exit` is `ok` or `fail`; `contains` and `absent` read the combined output (`absent` also
  searches every file in the tree, `absentInFiles` only the files); `unchanged` compares the tree
  before and after; `capture: "<prefix>"` keeps the word after the prefix as `{{secret}}` for
  later checks and never prints it. A wrong exit or output is `product`; a missing binary or
  variable is `env`.
- A `fields` path segment `key=value` picks the array element whose `key` is that string
  (`status.base.runtimes.name=python.version`).
- `publish-skill`'s `contains` takes one string or a list (all must be in the refusal).

## Node, plain-HTTP and VMM expectations

- `expect: { node: { fields: { status.guestMode: vm } } }` reads `GET /v1/node` and matches
  `fields` as dotted camelCase paths from the response body (exact value, a bool, or
  `{ contains: text }`). A mismatch is `product`.
- `expect: { plain-http: refused }` sends a request to vamd's address over plain `http://` with
  no key. It passes on a refused or dropped connection, or a non-2xx answer with no API body;
  a 2xx or an API body is `product`.
- `expect: { vmm: jailed }` (or `{ jailed: true, under: /srv/jailer }`) reads the host's
  `/proc` for the `firecracker` process: its root is under the jail directory (the jailer pivots
  into a new mount namespace, so `readlink /proc/<pid>/root` reads `/`; the directory the process
  opened is compared with the jail directories instead), its uid is not 0,
  and its PID and mount namespaces differ from the runner's. No such process is `product`
  after the step budget; an unreadable `/proc` entry is `env`.
- `expect: { vmm: { jailed: true, seccomp: filter } }` also requires `Seccomp: 2` (filter mode) in
  the Firecracker process's `/proc/<pid>/status`. Without `seccomp` the filter is not read, so
  other uses keep their meaning. A failure lists the value of each thread.
- A scenario may carry a `description`. A `needs: process` scenario needs a `mock:` block only
  when a step sends a model message (`chat`, `start-playbook`).

## Secrets, host-tool skills, floods and paging

- World kind `secret` (`api-token`): an org secret with a random value the runner makes and keeps
  in memory. The value is never printed, logged or put in a brief; it carries `/ + = & "` so
  percent and JSON encodings differ from the raw text. `{{name:<piece>}}` in a later piece is the
  secret's binding name.
- World kind `skill` (`skill-host-fixtures`): `body.spec` is the skill manifest. The runner packs it
  with a `SKILLS.md`, publishes it as a skill package (`POST /v1/skills/packages`), releases it as
  1.0.0, and an agent piece names it with `{{urn:<piece>}}` in `skillRefs` (and `{{name:<piece>}}`
  in a permission target). `PUT /v1/skills` alone does not reach a session. Tools reach the model
  only after `load_skill`, so a skill's name has no run tag (`<piece>-<scenario id>`): the mock
  script spells it out in a `load_skill` call. Host-tool manifests use the snake_case keys
  `host_exec`, `adapter_id`, `required_secrets` (the host reads those).
- A `secret` piece with an `egress:` block (`egress-token`) is an egress secret instead: it takes
  `bindingId` (fixed, not run-unique, because the guest's placeholder is `vamsec_` plus the id
  with `/` and `-` as `_` and a mock script must write it) and `allowedDestinations`.
- The fake origin `oversize.e2e.test` and the `egress_probe` adapter exist only in a `process-guest`
  `vamd` started with `VAMD_E2E_MOCK_SCRIPTS`: a default `vamd` does not compile them in
  (`scripts/e2e/check-vamd-build.sh` checks for `oversize.e2e.test`). The origin echoes the
  `Authorization` value it is sent, or the last one, repeated to `?size=N` bytes, over TLS signed
  by a CA made at start. The proxy uses a 64 KiB scrub buffer against it so a scenario stays small.
  Tool processes cannot open the proxy's port-55 socket in process mode, so the `egress_probe`
  host tool opens it and reports a verdict (`small-ok`, `oversize-refused`, `later-refused`,
  `leak-none`); the host redacts tool output, so the adapter checks the raw bytes itself.
- `requires: [piece]` makes a piece available to the `{{urn:..}}` and `{{name:..}}` placeholders.
- The fixture adapters `secret_echo`, `secret_forms` and `bulk_output` exist only on a node started with
  `VAMD_E2E_MOCK_SCRIPTS` (the runner always sets it). An ordinary node has neither.
- `expect: { secret-absent: { secret: api-token } }` searches the latest run's thread messages (all
  views), run events, receipt with its model appendix, and workspace files for the secret in every
  form (raw, base64 and base64url padded or not at three alignments, hex, percent-encoded,
  JSON-escaped) and for any run of 12 or more of its bytes. A hit is `product` and names the form
  and the place only, never the bytes.
- `expect: { thread: { role, contains, maxBytes } }`: the matched message's text is also at most
  `maxBytes` bytes. The observation lists the sizes seen.
- `flood: { as: <org alias>, path: /v1/node, count: 300 }` sends `count` GETs with that identity's
  key, in parallel on keep-alive connections, and records the answers by status.
  `expect: { flood: { rateLimited: { atLeast: N }, retryAfter: present, recovers: true } }` judges
  them: at least N answered 429, every 429 carries `Retry-After` in seconds, and after sleeping that
  long one request with the same key is answered 2xx. A failure lists the counts by status. Use a
  second identity so no other scenario's key is throttled.
- `GET /v1/approvals` is read page by page (`pagination.nextPageToken` as `pageToken`) when a
  node pages it.

## Triggers, Space and thread ids (`crates/vam-e2e/src/triggers.rs`)

World pieces:
- Kind `broker` (`trigger-test-broker`): starts `$VAM_E2E_TEST_BROKER` (`scripts/e2e/run.sh` copies
  the `hsp-echo-broker` binary to `e2e-bin` and exports the variable) on a short socket path
  (`/tmp/vtb-<pid>-<n>/b.sock`; a Unix path is limited to 108 bytes) and registers it with
  `PUT /v1/hosted/brokers`. `body` is the request; `{{socket}}` is the path. The broker id is
  fixed: a second broker exporting the same tool would collide in the node's tool catalog. The
  process is stopped, and its directory removed, when the scenario ends.
- Kind `workspace-seed` (no piece uses it since ids became the identity; the kind stays for a
  recorded fixture): copies a recorded Drive9 fixture
  (`e2e/world/fixtures/<fixture>/{workspaces,chunks}`) into the node's `drive9` store at arrange
  time (a chunk already present is kept). `workspaceId` and `thread` are the names the recording
  node gave. An agent or playbook piece may use a fixed urn when no other piece shares it.

Steps and expects:
- `fire-trigger: { key, times, kind: scheduled|run-now, label }` runs
  `$VAM_E2E_TEST_BROKER fire --state-dir <dir> --key <k> --times <n> --kind <kind>` and reads one
  JSON object per fire (`outcome`, `runId`, `denyReason`, `busyReason`). An `ACCEPTED` or `QUEUED`
  fire becomes the latest run, so `expect: { run }`, `{ file }`, `{ receipt }` and `{ thread }`
  follow it. `label` is the `e2e-step` label of the message the grant carries.
- `expect: { fire: { outcomes: [..], reason, sameRun } }` judges the latest `fire-trigger`.
- `expect: { grant: { tool, fields, count, absent } }` reads `GET /v1/keys?holder=broker` (rows
  whose `holder` starts `broker:`) and waits up to the settle time.
- `revoke-grant: { via: api }` is `DELETE /v1/keys/{id}` for the latest grant.
- `bearer-request: { method, path, token: grant|grant-secret, status }` sends one request with the
  grant read from `<broker dir>/grant-token` (never printed) as the bearer; the body is a real run
  start whose prompt carries the `bearer` label. Pair it with `expect: { runs: { count } }`.
- `expect: { runs: { count, latest: { fields } } }`: runs listed for the latest run's agent, and
  fields of the latest run's status.
- `chat: { thread: { id: latest } }` names the latest run's thread by the `th_...` id the thread
  list reports. `expect: { spaces: { workspace, fields } }` and
  `expect: { threads: { workspace, count, fields, by: name|id, messages } }` read `GET /v1/spaces`
  and the workspace's thread list (`workspace` is a `workspace-seed` alias, else the latest run's).

Test broker contract: `hsp-echo-broker --triggers --socket <dir>/b.sock` serves the tool
`echo_arm_trigger {run, after_ms?, times?}` (optional `--state-dir`, `--gateway-socket` for an armed
`after_ms` firing); on an Invoke that carries a grant it writes the whole token to
`<dir>/grant-token` (mode 0600) and answers with labels only. `fire --state-dir <dir>
--gateway-socket <p> --key K ...` requires the gateway socket: the runner reads it from
`gatewaySocket` of `GET /v1/hosted/brokers/trigger-test-broker` and the step fails with a clear
message while vamd reports it empty (until the gateway exists). Until vamd issues grants, the first
`expect: { grant }` of each trigger scenario is red.


## Schedules (`crates/vam-e2e/src/schedule.rs`)

The schedule broker (`vam-schedule-broker`) runs as a real process beside the process-mode node.
The model is the mock. Times are seconds ahead through a relative `once` (`when: {once: {in:
PT10S}}`), never cron, so no clock seam is needed; cron, DST and catch-up arithmetic are unit tests.

- Piece fields for kind `broker`: `bin` (the env var that names the binary; default
  `VAM_E2E_TEST_BROKER`; `schedule-broker` uses `VAM_E2E_SCHEDULE_BROKER`, which
  `scripts/e2e/run.sh` exports), `args` (the process arguments with `{{socket}}` and `{{dir}}`;
  default `--triggers --socket {{socket}}`) and `restartWithGateway: true` (vamd opens the broker's
  gateway only after the registration, so the runner reads `gatewaySocket` and starts the process
  again with `--gateway-socket <path>`). Cleanup removes the registration
  (`DELETE /v1/hosted/brokers/{id}`) of every broker piece, then stops the process.
- `wait-scheduled-run: { within, label }` waits (default 30s) for a run of the scenario's agent
  with `source.kind: trigger` and `source.brokerId: vam.schedule` that the scenario has not seen.
  It becomes the latest run, so `expect: {run | file | thread | receipt | runs | spaces}` follow
  it. `label` is the `e2e-step` label of the schedule's message, for the mock rules. None in time
  is `product`.
- `expect: { scheduled-runs: { count, within } }` watches the whole window (default 5s) and passes
  only if exactly `count` such runs exist at its end and never more.
- `stop-broker: {}` / `start-broker: {}` stop (SIGTERM, SIGKILL after 5s) and start the schedule
  broker again with the same dir, socket and gateway socket; no new registration.
- `{{schedule-id}}` in a `chat` text is the newest `sch_...` word in a tool result of the latest
  run's thread.
- Mock placeholder `{{last:<prefix>}}` in a reply's `toolCall.args` string values
  (`vam-inference/src/mock_script.rs`): the newest word that starts with `<prefix>` in the
  request's user and tool messages. Mock only.

Results the scenarios read (the contract the tools must meet; a change is a spec change):

- `schedule_create` answers with `scheduleId` (`sch_...`), so a tool message contains `sch_`.
- `schedule_list` answers `{"schedules":[...]}`; for a caller with none, exactly
  `"schedules":[]` appears in the result text.
- `schedule_get` of a schedule that is not the caller's answers the typed error `NOT_FOUND`; of
  the caller's own it answers the schedule with a `fires` list whose entries carry `outcome`, and a
  fire skipped because the workspace was busy has `"outcome":"skipped_busy"`.
- `schedule_delete` revokes the grant: the grant row in `GET /v1/keys?holder=broker` has
  `revokedAt` set.

Until the tools exist the stub answers every call with `UNIMPLEMENTED`, so each schedule scenario
is red at the first check of a result (`class product`).

## Effects the world counts (`crates/vam-e2e/src/effects.rs`)

The `effects-test-broker` piece is the `hsp-echo-broker` binary started with `--effects`
(`crates/vam-orchestrator/src/hsp_echo_effects.rs`). Tools: `echo_create_ticket {title, holdMs?}` and
`echo_create_note {title, holdMs?}` (mutating) and `echo_find_ticket {title}` (read). Every call
appends a line to `<dir>/ledger.jsonl` and syncs it BEFORE the call holds for `holdMs` and answers,
so a kill during the hold leaves an effect that happened with no answer. The broker is its own
process: the ledger survives a guest kill and a vamd restart, and its registration survives a vamd
restart. `hsp-echo-broker ledger --state-dir <dir>` prints one JSON line per tool and title with its
count. Duplicates are counted here, never by the journal under test.

- `kill-guest: {}` sends SIGKILL to the latest run's process guest (found through the pid files
  under `/tmp/vam-process-guest` and the node's state dir) and waits until it is gone. A crash, never
  a stop. Needs process mode; no live guest is `env`.
- `expect: { ledger: { piece, tool, title?, count } }` waits (up to 20s) until the named broker piece's
  ledger holds exactly `count` lines for the tool (and title); a different count is `product`.
- Event names with no emitter yet: `effect.recorded`, `effect.replayed`, `effect.reconciled`,
  `run.resumed`, `model.fallback`.

## Outcomes per agent version (`crates/vam-e2e/src/outcomes.rs`)

What receipts add up to per agent version, and the steps that move an agent between versions.
`piece` is the alias of an agent piece in `given`. Every `fields` takes the matchers above; numbers
compare by value (`1` matches `1.0`), in every `fields` family.

- `expect: { agent-stats: { piece, version?, k?, fields } }` reads
  `GET /v1/agents/{urn}/outcomes[?version=&k=]` (`version`: `X.Y.Z` or `draft`; omitted lists the
  versions that have runs) and matches `fields` as dotted camelCase paths into the answer
  (`passK.value`, `effects.duplicates`, `costMissing.0`). A non-2xx answer is `product`.
- `expect: { release-diff: { piece, version, k?, fields } }` reads
  `GET /v1/agents/{urn}/outcomes/diff?version=` (`base.*`, `candidate.*`, `changes.*`). `product`
  on a non-2xx.
- `edit-agent-draft: { piece, set: { <dotted spec path>: value } }` reads the piece's draft
  (`GET /v1/agents/{urn}`), sets the paths inside its `spec` and puts it back (`PUT /v1/agents`).
  A non-2xx is `harness`; a path through a scalar is `scenario`. The edit changes the draft's
  digest, so later `chat { draft: true }` runs form a new draft group.
- `release-agent: { piece, version, refused? }` posts the release (`version` like `1.1.0`).
  `refused: true` passes only on a 4xx; a refused plain release is `harness`.
- `verify-receipt: { file, trustedKeys?, tamper? }` verifies a receipt file saved earlier (a path
  from the repo root) with the built `vam receipt verify`, pinned with `--trusted-keys` when given.
  `tamper` changes one byte in a copy and wants a failure naming the signature.

Fixtures under `e2e/world/fixtures/receipts/`: `receipt-effects-not-journaled.json` is a receipt a
node issued while receipts said "effects not journaled on this version" (a real run of the
`agent-with-builtins` write scenario on the base before effects were listed), and `node-keys.json`
is that node's public key in the `vam node keys` shape (`signingKeys[]`, no secret). Never
regenerate them: an old receipt that stops verifying is the failure they exist to catch.

## Notifications (`crates/vam-e2e/src/notifications.rs`)

The user's notification list, read through the API as one identity. A route that answers 4xx fails the step at once
(`product`); a row that is not there yet is waited for until `within` (the step budget when omitted).

- `expect: { notifications: { kind, count, run, unread, state, fields, absent, as, within, approval, digest } }`
  reads `GET /v1/notifications?state=` (all pages) as `as` (the bootstrap identity when omitted). `kind` and
  `run: latest` and `unread` pick the matching rows; `count` is exact (at least one when omitted); `absent: true` needs
  none (it fails as soon as one is there, so wait for the thing that would write it first). `fields` are dotted
  camelCase paths over the first matching row, with the same matchers as `receipt` fields. `state` is `open`
  (unresolved) or `all` (default). `approval: latest` needs the row's `approvalId` to be the request id of the latest
  run's pending approval; `digest: receipt` needs its `receiptDigest` to equal the latest run's
  `signature.receiptDigest`.
- `expect: { notification-summary: { action, warn, unread, as, within } }` reads `GET /v1/notifications/summary`.
- `notification-mark: { action: read|acted|get, kind, as, of, status, refused, contains }` takes the first record (of
  `kind`) in the list of `of` (the bootstrap identity when omitted) and calls `/read`, `/acted` or, for `get`, `GET`
  on its id as `as`. It passes on a 2xx answer, on exactly `status` when given, or on any 4xx with `refused: true`;
  `contains` looks in the answer body.
- `via: cli|app` on `approve`, `deny` and `decide-gate`: `cli` runs the built `vam approve decide` (`VAM_E2E_CLI`,
  with `--host`, `--token` and `VAM_CA_FILE`; the CLI sends `X-Vam-Surface: cli`), `app` sends `X-Vam-Surface: app`.
  Without `via` the runner sends no surface header, so the surface stays `api`.
- Tag `notifications`.

**Benchmark probes** (mock only, written to `summary.json` under `bench.notify`; never fail a scenario). After every
`expect: { approval: pending }` or `{ gate: pending }` the runner reads the run's pending approvals and gates and the
open list, and after those and every decision it reads the whole list.
- `pendingListed {listed, pending}`: pending approvals and gates that an unresolved record's `approvalId` names, over
  all pending ones seen in the run (the P2 line; must reach 100%).
- `shownWithin60s {within, total}`: action records with `firstShownAt - createdAt <= 60`. The runner looks at the
  summary once after parking and lists when `action` is above zero, as the app does when its badge rises; it does not
  wait 15 seconds between polls (the P7 line, host side only).
- `medianToActSecs`: median of `actedAt - createdAt`; the scenario script's own pace, so recorded and never scored.
- Both of the last two read `"unmeasured"` while no action record has been seen (the routes answer 404).

## Channels (`crates/vam-e2e/src/channels.rs`)

Slack through the fake transport of `vam-notify-broker` (the `notify-broker` piece; `scripts/e2e/run.sh` builds the
fake-transport binary and exports `VAM_E2E_NOTIFY_BROKER`). No Slack call leaves the process: each Web API call the
broker would make is one line `{"method","body"}` in `<broker dir>/state/sent.jsonl`, the token removed. A message
has `body.channel` (the Slack user id), `body.blocks` (Block Kit) and `body.metadata.event_type` (`vam.<kind>`).
List `trigger-test-broker` before `notify-broker` in `given` when both are used (`fire-trigger` reads the first
broker started). Tag `notifications`; a scenario that waits for the wall clock also carries `slow` and is left out of
`--smoke`.

- `node-notify: { slack: { botToken, appToken }, appUrl }`: `PUT /v1/node/notify` with
  `{ slack: { enabled: true, botTokenRef, appTokenRef }, appUrl }`; the refs are the vault names of the two `secret`
  pieces.
- `link-contact: { channel, account, as, verify }`: `POST /v1/me/contacts` `{ channel, address }` as `as`; with
  `verify` (the default) the runner takes the code from the response (ten Crockford base32 symbols, `XXXXX-XXXXX`;
  shown once, design 3.6: the code goes user to bot) and writes it as the account with `vam-notify-broker fake dm`. `verify: false` leaves the contact unconfirmed.
- `unlink-contact: { channel, as }`: `GET /v1/me/contacts`, then `POST /v1/me/contacts/{id}/unlink`.
- `notify-prefs: { quietHours, always, as }`: `PUT /v1/me/notification-prefs` with `{ quietHours: { start, end, tz },
  always: [kinds] }`. Quiet hours are `HH:MM` in an IANA zone, so there is no seconds value. `quietHours: now` is
  runner shorthand for the next whole UTC minute (start = the current minute, end = one minute later; near the end
  of a minute the runner waits for the next one first). A scenario that uses it waits for the window with
  `within: 150s`.
- `channel-click: { account, message, action, handle, via-socket, status }`: a Slack user presses a button. The runner
  runs `vam-notify-broker fake click --state-dir <state> --account <A> --message <kind|latest> --action <a>
  [--handle forged|replay|from:<account>] [--gateway-socket <path>]`, which talks to the running broker over
  `<state>/fake.sock` and prints one JSON object `{"status":"OK|INVALID|NOT_LINKED|NOT_PERMITTED|ALREADY_RESOLVED",
  "detail":""}`. Exit 0 whenever the click reached `ActOnNotification`. `--message` picks the latest message of that
  kind sent to the account that the broker holds a handle for. `--handle` omitted uses that message's own handle;
  `forged` is a handle vamd never issued; `replay` is the handle of the last click again; `from:<account>` is the
  handle of the latest message of that kind sent to that other account. `via-socket: <broker id>` sends the same
  call through that broker's gateway socket (`gatewaySocket` of `GET /v1/hosted/brokers/{id}`) instead of the
  notification broker's own. `status` is the answer wanted (`OK` when omitted). `fake dm --state-dir <state>
  --account <A> --text <t>` is a user writing to the app.
  `--action` is the id of a button (`approve`, `deny`, `opt_0`) or, for convenience, the label of a gate option
  (`go`), mapped to the button's id. The broker tells vamd the gate option id as rendered in `RenderedAction.option`, never the label. `--message` falls back to the latest message of that kind to anyone when none went to `--account` (a click
  from an unlinked account). A click on an action the message never offered answers `INVALID` without calling
  `vamd`. `fake dm` exits 0 for a code that linked (`LINKED`) and for a DM that holds no code (`IGNORED`), 1 for any
  other link outcome. `fake handles --state-dir <state>` prints a JSON list of `{delivery_id, kind, account, actions,
  handle_held}` (never a handle value). `fake act-raw --state-dir <state> --account <A> --handle <h> --decision <d>
  [--gateway-socket <path>]` calls `ActOnNotification` with that handle and decision, no message needed.
  `node-notify` first unlinks the owner's Slack contacts left by an earlier scenario of the same run (one vamd, one
  bootstrap owner).
- `expect: { channel-messages: { to, kind, count, buttons, openInVam, inputs, updated, mentions, absent, within } }`
  reads `sent.jsonl`. `to` is the account (`T1/U1`) or its user id; `kind` the message kind; `count` the number of
  `chat.postMessage` calls; `buttons`, `inputs`, `openInVam` look at the last message (the last `chat.update` when
  `updated: true`); `inputs` counts input elements; `openInVam` looks for the text `Open in VAM`; `mentions` is a word
  the last update's text must hold (the surface that decided); `absent: true` needs no message. A mismatch is waited
  for until `within` (the step budget when omitted), then fails as `product`.
- `expect: { broker-state: { piece, lacks } }`: no file in the broker's state directory holds the value of a secret
  piece named in `lacks`, or a literal text (`vam_na_`). It fails when the directory is empty, so a broker that wrote
  nothing cannot pass.

**Benchmark probes** (mock only, written to `summary.json` under `bench.notify`, never fail a scenario):
`channelShownWithin60s {within, total}` and `channelMedianSecs` (record `createdAt` to `firstShownAt` for the kinds a
`channel-messages` expect saw; a delivery held by quiet hours is not counted) and `triggeredDecidedFromChannel
{completed, total}` (a trigger-started run decided by a good `channel-click` that then succeeds). All read
`"unmeasured"` until a message was seen.

## Connectors (`crates/vam-e2e/src/connectors.rs`)

Connector scenarios use two runner-hosted loopback providers (std-only HTTP, started once per run when a
scenario `given`s a `mock-provider` piece). Tag `connectors`. Token values are made by the runner, kept in memory,
added to the scrub list and never printed or logged; a listing that holds one fails the `connectors` expect.

- World piece `kind: mock-provider`, `provider: microsoft-login | microsoft-graph | linear-server`. `microsoft-login` serves
  `POST /{tenant}/oauth2/v2.0/token` for `refresh_token` grants it was told about, logs form field **names** (and the
  values of `client_id`, `grant_type`, `scope` only), and answers `invalid_grant` once revoked. `linear-server` is a
  streamable-HTTP MCP server at `/mcp` (`list_issues`, `get_issue`, `save_issue`, `save_comment` and the unvetted
  `export_workspace`) that needs the runner-made bearer. A piece resets its provider at the start of each scenario.
- World piece `org-administrator` (`kind: org`, `role: OrgAdmin`): a second identity with `users:admin` and not the
  operator key. It is not `org-admin`, which is the bootstrap key (role Owner).
- `connector-node: { id, enabled, consent: { tenant }, as, refused }`: `PUT /v1/connectors/{id}/node` with
  `{ enabled, consent: { tenantId } }`. `as` is an `org` alias (the bootstrap key when omitted).
- `connector-connect: { id, url, piece, tenant, upn, as, refused }`: connects as the app would. `microsoft-365` uses
  `PUT /v1/connectors/microsoft-365/connection` with `{ accessToken, refreshToken, idToken }`; the id token is
  unsigned and carries `tid` (`tenant`), `preferred_username` (`upn`) and `iss`. Any other id uses the MCP routes
  (`PUT /v1/mcp/connections`, then `…/oauth/tokens`) with the id, the `url` (or the provider's address with `piece`)
  and, for the Linear mock, its bearer. Rows made (MCP rows, and the `microsoft-365` connection by `DELETE …/connection`) are removed at cleanup as the same identity.
- `connector-check: { id, as }`: `POST /v1/connectors/{id}/connection/check`; must answer 2xx.
- `mock-provider: { piece, revoke }`: with `revoke: true` the provider refuses everything from then on.
- `expect: { connectors: { id, as, fields, absent } }`: the entry of `GET /v1/connectors` for the caller; `fields`
  are dotted paths with exact values, `absent` are paths that must not exist. A 404 or a missing entry is `product`.
- `expect: { mock-requests: { piece, path, has, lacks, count } }`: what the provider saw at `path`. `has`/`lacks`
  name fields: a form field (`resource`, `client_secret`), `name=value` for the logged non-secret ones, or for Linear
  `authorization`, `authorization-wrong`, `method=<rpc>`, `tool=<name>`.
- `refused: true` on these steps means the answer is 400, 403 or 409. A 404 or 405 is a missing route and fails
  the step as `product`; other answers fail it too.
- The runner gives `vamd` `VAMD_CONNECTOR_MICROSOFT_365_CLIENT_ID` and `VAMD_PROCESS_CONNECTOR_ENDPOINTS` whenever the
  mocks run (`PASS_CONNECTOR_ENDPOINTS` is on). The second is a JSON map `{"microsoft-login": <base url>,
  "linear-server": <mcp url>}` of plain-http loopback URLs. It exists only in a `process-guest` build, needs
  `VAMD_PROCESS_GUEST_BIN`, and a default `vamd` refuses it like every `VAMD_PROCESS_*` variable
  (`scripts/e2e/check-vamd-build.sh` proves the marker is absent). It maps Linear's own MCP URL and the Entra
  authority host (`login.microsoftonline.com`) to the mocks, for the MCP client and the token refresh; every other URL
  keeps the SSRF rules.

### Skills that call Microsoft Graph as the member

- `mock-provider` with `provider: microsoft-graph` serves `GET /v1.0/me/messages` (two fixed messages) and
  `POST /v1.0/me/sendMail` (202), and 404 for the rest. It answers 401 unless the bearer is the login mock's current
  access token. It logs `method=<verb>` and `bearer=current|stale|placeholder|unknown|none` per request, never a value.
  The login mock tracks the token a connect or a refresh made current; a token handed over with `expiresIn: 0` is
  stale from the start.
- `connector-connect` takes `expiresIn` (seconds, `microsoft-365`): sent as `expiresIn` in the connection body.
  `expect: { secret-absent: { secret: microsoft-365 } }` judges the connection's access and refresh tokens.
- `mock-requests` takes `method` and `path: "*"` (any path), and `count: 0` passes when nothing was seen.
- `skill` pieces take `files: { <path in the package>: <path under e2e/world/fixtures> }`, packed next to `SKILLS.md`
  and `manifest.json`. `skill-m365-probe` uses it for `probe.py` (python3 stdlib): it reads `{ method, path }` from
  stdin and calls Graph through the guest proxy with `X-Target-Url` and only the placeholder `vamsec_m365_graph`.
- Runner switches: `PASS_SKILL_CONNECTOR_FIELDS` (`forms.rs`: `requiresConnector` and per-tool `egress` in a
  published manifest) has been on since CN-2 W1, and `PASS_GRAPH_ENDPOINT` (`connectors.rs`: the `microsoft-graph`
  key of the seam, served by the egress gate for `graph.microsoft.com`) since CN-2 W2.
- A call that waits for the member to connect or reconnect is observed as `expect: { run: paused }` plus
  `expect: { approval: pending, tool: <tool> }`. A refusal by the egress envelope is observed as a tool message
  containing `declared shape`.

## Cards (skills that name a card)

Scenarios for a skill whose tools name a card (`ui.show` for a result, `ui.ask` for an approval).

- `skill` pieces take `deferred: true`: the runner works out the piece's name and URN at `given` time but does not
  publish it. A `publish-skill` step does, so a refusal at publish is a step result (class `product`), not a harness
  failure while the world is built. An agent piece that `requires` a deferred skill is also `deferred: true` (it is saved at `given` time
  and released by the `publish-skill` step that releases the skill, since vamd refuses to release an agent whose skill is
  not released).
- `publish-skill: { piece, refused, contains }`: packs the deferred skill piece (its `files:` included, as for any
  skill piece), publishes it and, when accepted, releases it as 1.0.0. With `refused: true` the answer must be 4xx and
  its body must contain `contains` (a tool name, a card file path or a JSON pointer).
- `approve: { amend: { <field>: <value> }, refused, as, via }`: decides AMENDED with `amendedArgs`. With
  `refused: true` the answer must be 4xx and the approval must stay pending (an edit of a field the card does not
  offer). Without it the decision must be accepted and the run must leave PAUSED.
- Card files are fixtures under `e2e/world/fixtures/mail-calendar-cards/jigs/` and are packed to `jigs/<name>.json`
  in the package. Expectations read the card through events (`expect: { event: outbox.card | card.skipped, fields }`),
  the pending approval's claims (`expect: { approval: pending, tool, fields: { card.data.<path> } }`) and the receipt.

## Marketplace (`crates/vam-e2e/src/marketplace.rs`)

Scenarios for installing artifacts from a Git marketplace. Tags `connectors`, `members`. They run in process mode
with the mock model. The runner makes the repositories; vamd reaches them through a process-only seam, so the
default vamd still accepts only `https://`.

**World pieces.** `kind: git-marketplace`, with `fixture` (a directory under `e2e/world/fixtures`), `url` (the
https URL the node is told about), `tags` (lightweight tags on the first commit), `preconfigured: true` (the node
lists it from the start under the id `squrell`; nobody adds it) and `publisherDomain` (what the admin confirms
when adding it). The runner copies the fixture, makes a repository on `main`, commits and tags it, and clones it
bare to `<out>/marketplaces/<piece>/bare.git`. It is made again at the start of every scenario that gives the piece.
Commits are deterministic (fixed author, committer and dates, no host config, no hooks), so the same fixture has the
same commit SHAs in every scenario: a node that remembers a tag's commit must not see it move between scenarios.

- `marketplace-squrell`: id `squrell`, `https://github.com/squrell-inc/vam-marketplace`, tag `microsoft-365-v1.0.0`.
  Fixture `marketplace-squrell/` is a vendored copy of the public `squrell-inc/vam-marketplace` repository (the
  real `m365-mail` skill in Bun TypeScript, the Executive assistant, the Morning triage playbook, README, LICENSE,
  CI workflow and Bun tests). Never edit it by hand: change the marketplace, copy it here and update `SOURCE`
  (marketplace commit, tag and git tree hash); `scripts/marketplace/check-snapshot.sh` (in `local.sh --fast`)
  fails when the copy drifts from its stamp. Process-mode tools find the guest's egress proxy in
  `VAM_EGRESS_ENDPOINT` (bun gets no `HTTP_PROXY`).
  Overlays: `marketplace-squrell-update/` (the index names `microsoft-365-v1.1.0`, one playbook text changes),
  `marketplace-squrell-moved-tag/` (one file that changes, for a re-pointed tag).
- `marketplace-community`: added by an admin. Four listings: `helpful-notes` (good: a skill whose egress declares a
  `/` prefix on `notes.example.test`, a playbook, an agent, all under `market.community.example`),
  `with-a-connector`, `claims-the-squrell-name` (an artifact under `hub.squrell.cloud`) and `claims-the-org-name`
  (an artifact under `local.vam.dev`, the e2e registry domain). Overlay `marketplace-community-broken/` (an index
  that is not JSON).
- `marketplace-community-connectors`: added by an admin, domain `connectors.community.example`. Four listings that ship a
  connector file (`connector.json`, the `spec/connectors/*.json` format): `bugwatch-connector` (installs: remote MCP at
  `https://mcp.bugwatch.example/mcp` with discovery) and three that must be refused with a reason
  (`reserved-connector-id` takes the id `linear`, `unknown-profile-connector` names the profile `generic`,
  `connector-with-a-secret` asks for `squrell_relay`). Overlay `marketplace-community-connectors-update/` is the tag
  `bugwatch-connector-v1.1.0` whose connector asks for one more scope and one more tool. The runner maps the Bugwatch MCP
  URL to the `linear-server` mock through the `mcp:<https url>` key of the connector seam (`PASS_MCP_SERVER_SEAM`), so
  `connector-connect: { id: bugwatch, url: "https://mcp.bugwatch.example/mcp", piece: mock-linear-server }` connects the
  installed connector through the MCP routes. `expect: { connectors }` reads array indexes (`provider.scopes.2`).

**The seam (on since CN-4.4).** `PASS_MARKETPLACE_SEAM` (`connectors.rs`) is `true`: the runner gives vamd
`VAMD_PROCESS_MARKETPLACE_URLS`, a JSON map from each piece's `url` to its `file://…/bare.git`. It exists only in a
process-guest build (a default vamd refuses it like every `VAMD_PROCESS_*` variable and has no `file://` transport; the
release gate checks both). A process-guest vamd does not read marketplaces on a timer: every read is a step.

**Steps.** `refused: true` means 400, 403 or 409 (a 404 or 405 is a missing route and fails as `product`);
`contains` is text the refusal's body must hold. `as` is an `org` alias (the bootstrap key when omitted).
`marketplace` is the alias of a `git-marketplace` piece in `given`.

- `git-tag: { piece, tag, move, overlay }`: adds a tag. With `overlay` (a fixture directory) its files are copied
  over the work tree and committed on `main` first, so the tag names a new commit and the index at the branch head
  changes. `move: true` lets an existing tag move to the new commit (or to HEAD without an overlay).
- `marketplace-add: { piece, as, refused, contains }`: `POST /v1/marketplaces` with `{ url, publisherDomain }`.
- `marketplace-refresh: { marketplace, as, refused, contains }`: `POST /v1/marketplaces/{id}/refresh`.
- `marketplace-install: { marketplace, listing, as, refused, contains }`:
  `POST /v1/marketplaces/{id}/listings/{listing}/install`.
- `marketplace-update: { marketplace, listing, as, refused, contains }`:
  `POST /v1/marketplaces/{id}/listings/{listing}/update`.
- `catalog-write: { kind, urn, action, as, refused, contains }`: `kind` is `agent | playbook | skill`, `action` is
  `put` (agent, playbook: `PUT /v1/{agents|playbooks}` with a minimal body), `release`
  (`POST …/{urn}/release`; skills under `/v1/skills/packages/{urn}`) or `delete`. `refused` defaults to **true**:
  nobody may change an installed row.
- `chat.agent` also takes a literal `urn:vam:agent:…` (an agent a marketplace installed).
- Cleanup removes the marketplaces the steps added (`DELETE /v1/marketplaces/{id}` as the same identity).

**Expects.** Fields are dotted paths (numbers index arrays) with exact values, `{ present: true }` or
`{ contains: text }`; `absent` are paths that must not exist.

- `marketplaces: { marketplace, as, fields, absent }`: the entry of `GET /v1/marketplaces`.
- `listings: { marketplace, listing, as, fields, absent }`: the entry of `GET /v1/marketplaces/{id}/listings`.
- `catalog: { kind, urnPrefix, as, count, fields }`: the artifacts whose URN starts with `urnPrefix`, read from
  `GET /v1/agents` and `GET /v1/playbooks` (`records`) and `GET /v1/skills/packages` (`packages`). The lists hold every
  release of an artifact, so an artifact counts once, at its newest release (a draft is not counted); `fields` hold on
  each. Records carry `tier` (same value as `catalogTier`), `readOnly` and `origin` (see below).
- `mock-requests.path` also takes a leading or trailing `*` (`"*/send"`, `"/v1.0/me/messages/*"`); a request that has
  not arrived yet is waited for (8 s) unless `count: 0`.

**Routes the scenarios expect** (written here so the waves that build them agree; all `users:admin` only; shapes
follow `/v1/connectors`):

| Route | Answer |
|---|---|
| `GET /v1/marketplaces` | `{ marketplaces: [{ id, name, url, publisherDomain, trust: squrell \| unverified, preconfigured, enabled, lastCheckedAt, lastError, indexDigest, tagsSeen: [{ tag, commit }] }], gitAvailable }`. `lastCheckedAt`, `lastError` and `indexDigest` are absent until set. The Squrell entry (`id: squrell`) is there from boot. |
| `POST /v1/marketplaces` | `{ url, publisherDomain }`; 201 `{ marketplace }`. The URL is https only (no user info, port, query; public addresses only). The domain must equal the index's and must not be a domain the node reserves (Squrell's, the org's, `local.vam.dev`, `*.local`, another marketplace's) or one the catalog already publishes under; the first read must be clean or nothing is added. 400 refused, 409 duplicate or catalog clash. |
| `PUT /v1/marketplaces/{id}` | `{ enabled }`. |
| `DELETE /v1/marketplaces/{id}` | Added ones only. |
| `POST /v1/marketplaces/{id}/refresh` | Reads the index and the tags only (installs nothing). 200 `{ marketplace }`; a broken index answers 200 with `lastError` set on that marketplace only; a tag that now names another commit is refused (409, text says "moved"). |
| `GET /v1/marketplaces/{id}/listings` | `{ listings: [{ id, displayName, description, ref, commit, requiresNode, connector, artifacts: [{ kind, path, urn }], installed: null \| { ref, commit }, updateAvailable, summary, error }] }`. `summary` is `{ artifacts: [capability summary per artifact], tools: [{ skill, name, sideEffect, approvalRequired, showCard?, askCard? }], egress: [{ host, methods, paths: [{ path, exact, anyPath }], anyPath, canWrite, writesByPrefix, bindings, text }], connector, requiresConnector }`; `egress` has one entry per host and `anyPath` is true when a `/` prefix is declared. `error` (a missing or moved tag, a failing package, an artifact outside the publisher domain, a node too old) means the listing cannot be installed as it stands. A disabled marketplace lists none. |
| `POST …/listings/{listing}/install` | Installs the index's `ref` as the last refresh read it. Refused with a reason for a listing with a connector (non-Squrell), an artifact URN under a reserved domain, or a moved tag. A repeat at the same tag is a no-op. |
| `POST …/listings/{listing}/update` | The same at the new `ref`. |

Installed catalog rows carry `tier: marketplace`, `readOnly: true` and `origin: { marketplace, listing, ref, commit,
packageDigest }`.

**Order independence.** One vamd serves the whole run, so installs would outlive a scenario. Every marketplace
scenario gives the same result alone, in a batch and in any order, because the runner resets the node's marketplace
state through a process-only seam: `POST /v1/process/marketplace-reset` (bootstrap key; compiled only into a
`process-guest` vamd and answered only in process mode; the release gate proves the path is absent from a default vamd).
It deletes every catalog row of tier `marketplace` with its origin and install records, and clears the read state of
the preconfigured Squrell entry (re-enabled, unread). It runs when a scenario with a `git-marketplace` piece starts
and again at its cleanup, before the added marketplaces are deleted. Pins and the git cache are kept: the runner's
repositories are deterministic, so a tag has the same commit in every scenario. Nobody else can remove an install:
`DELETE /v1/marketplaces/{id}` is refused while one exists (409), and a catalog `DELETE` answers 409 for an installed
URN and 405 for any other (the catalog keeps every release).

**Installed rows.** `GET /v1/agents`, `/v1/playbooks` and `/v1/skills/packages` rows (and `GET /v1/agents/{urn}`)
carry `tier` (`user | platform | marketplace`, same as `catalogTier`), `readOnly` (true for `marketplace`) and, for
installed rows, `origin: { marketplace, listing, ref, commit, packageDigest }`. An agent or playbook that changes
between tags is a new release (next patch); the unchanged artifacts keep their release and their `origin` follows the
new tag.

**Test-only create tool.** `skill-m365-overreach` / `agent-with-m365-overreach` (deferred; published by a
`publish-skill` step) declare `POST` on exactly `/v1.0/me/messages` and then try `POST /v1.0/me/messages/<id>/send`.
The refusal ("declared shape", Graph saw no send) turns green with exact paths in the egress envelope.
The skill piece's field is `egress[].paths` (exact, proto `ToolEgress.paths = 5`); the refusal text names the declared
shape as `POST graph.microsoft.com exactly /v1.0/me/messages`. A write method declared with `pathPrefixes` still
publishes, with a warning, and the capability summary sets `writesByPrefix` on that egress entry.

**Graph mock** (`mock-microsoft-graph`): besides `GET /v1.0/me/messages` and `POST /v1.0/me/sendMail` it serves
`$search` on messages (logged as the field `search`), `GET /v1.0/me/messages/{id}`, `GET /v1.0/me/calendarView`,
`POST /v1.0/me/findMeetingTimes`, `POST /v1.0/me/messages` (a draft, id `d1`), `POST …/messages/{id}/send`,
`POST /v1.0/me/events` and `POST …/events/{id}/forward`, all logged with `method=` and `bearer=`. The send and forward routes
are accepted on purpose, so a refusal comes from the envelope and not from a missing route.
`mock-provider: { piece, hold: true }` makes `sendMail` log the request and then wait; `hold: false` releases it.
