//! `vam-e2e run`: select, start a throwaway vamd, run each scenario, write the outputs.

use crate::api::Api;
use crate::catalog::Catalog;
use crate::driver::{run_scenario, ScenarioResult};
use crate::engine::{parse_duration, Cfg, Engine};
use crate::runner::RunTag;
use crate::scenario::{Needs, Scenario, Status};
use crate::vamd::{Vamd, VamdConfig, VamdProcess};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct RunOpts {
    pub root: PathBuf,
    pub scenarios: Vec<String>,
    pub tags: Vec<String>,
    pub all: bool,
    pub smoke: bool,
    pub firecracker: bool,
    pub budget: Duration,
    /// `--step-budget`: overrides the step budget of any run.
    pub step_budget: Option<Duration>,
    /// `--scenario-budget`: overrides the scenario budget of any run.
    pub scenario_budget: Option<Duration>,
    pub out: Option<PathBuf>,
    pub bin_dir: Option<PathBuf>,
    pub keep: bool,
    /// A persistent vamd state dir (the node's vault lives there). Needs
    /// `VAMD_SECRETS_MASTER_KEY` in the environment; the node's admin key is kept in
    /// `<dir>/node-key`.
    pub state_dir: Option<PathBuf>,
    /// `--bench`: run the `model` scenarios k times each and write `bench-results.json`.
    pub bench: Option<BenchOpts>,
}

/// The `--bench` flags.
#[derive(Debug, Clone, Default)]
pub struct BenchOpts {
    /// Use the scenario's `mock` block and `mock-model`; the results are never publishable.
    pub mock: bool,
    /// Allow a model other than `mock-model` (it spends money).
    pub real: bool,
    /// Catalog models to run; `mock-model` with `--mock`.
    pub models: Vec<String>,
    /// `direct` (the maker's endpoint) or `vercel` (the Vercel AI Gateway, transport only).
    pub provider: String,
    /// Aggregate recorded trials instead of running anything.
    pub recorded: Option<PathBuf>,
    /// Require this task-set version (it must be pinned in `task-sets.yaml`).
    pub task_set_version: Option<String>,
    /// Trials per (task, model), over the scenario's `model.k`.
    pub k: Option<u32>,
    /// The harness profile every trial's chat names; none sends no profile (`base`).
    pub profile: Option<String>,
    /// Stop starting trials once the run's cost passes this many micro-USD.
    pub max_usd_micro: Option<i64>,
}

/// A profile name must be one `vam-spec` knows; refused before any trial (nothing is spent).
pub fn check_profile(name: &str) -> Result<(), String> {
    if vam_spec::harness::profile(name).is_some() {
        return Ok(());
    }
    let known: Vec<&str> = vam_spec::harness::PROFILES.iter().map(|p| p.name).collect();
    Err(format!(
        "unknown --profile `{name}` (profiles: {})",
        known.join(", ")
    ))
}

/// The scenario layer a node of this mode runs.
pub fn node_needs(firecracker: bool) -> Needs {
    if firecracker {
        Needs::Firecracker
    } else {
        Needs::Process
    }
}

/// The effective uid in the text of `/proc/<pid>/status` (`Uid: real effective saved fs`).
pub fn euid_from_status(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The microVM lane runs vamd with the jailer on, which needs root (and the VMM probe
/// reads `/proc/<pid>/root` of a process that is not the runner's). Class `env`:
/// nothing about the product is wrong, the lane was started wrongly.
pub fn require_root_for_jailer(euid: Option<u32>) -> Result<(), String> {
    match euid {
        Some(0) => Ok(()),
        other => Err(format!(
            "--firecracker needs root (euid is {}): the node runs the jailer, which needs it. \
             Build as your user, then run with `sudo env \"PATH=$PATH\" <the VAMD_*/VAM_* variables> VAM_E2E_SKIP_BUILD=1 \
             VAM_E2E_VAMD=<a default vamd build> scripts/e2e/run.sh --firecracker` (class env)",
            other
                .map(|u| u.to_string())
                .unwrap_or_else(|| "unknown".into())
        )),
    }
}

/// The lane assets vamd needs for microVMs, from the environment (the `VAMD_*` name, else its
/// `VAM_*` twin). Every one must be an existing file.
pub fn lane_assets() -> Result<Vec<(String, String)>, String> {
    lane_assets_from(|k| std::env::var(k).ok())
}

pub fn lane_assets_from(
    get: impl Fn(&str) -> Option<String>,
) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (vamd_name, alt) in [
        ("VAMD_FIRECRACKER_BIN", Some("VAM_FIRECRACKER_BIN")),
        ("VAMD_JAILER_BIN", Some("VAM_JAILER_BIN")),
        ("VAM_KERNEL_PATH", None),
        ("VAM_ROOTFS_PATH", None),
    ] {
        let v = get(vamd_name)
            .or_else(|| alt.and_then(&get))
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                format!("--firecracker needs {vamd_name} (scripts/e2e/firecracker-assets.sh)")
            })?;
        if !std::path::Path::new(&v).is_file() {
            return Err(format!("{vamd_name}={v} is not a file"));
        }
        out.push((vamd_name.to_string(), v));
    }
    Ok(out)
}

/// Lane diagnostics switches the runner forwards to `vamd` (which starts from a cleared
/// environment): the guest serial console, keeping each VMM's stdout/stderr, the host
/// phase timer, and the golden bake soak (re-bakes at startup, lane only).
pub fn lane_diag_env_from(get: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    [
        "VAM_SERIAL_CONSOLE",
        "VAM_KEEP_VM_LOGS",
        "VAM_TIMING",
        "VAM_GOLDEN_BAKE_SOAK",
    ]
    .into_iter()
    .filter_map(|k| get(k).filter(|v| !v.is_empty()).map(|v| (k.to_string(), v)))
    .collect()
}

/// How long the runner waits for `vamd` to answer in microVM mode: 90 s for the first
/// bake, plus 30 s per soak re-bake when `VAM_GOLDEN_BAKE_SOAK` is a positive number.
pub fn lane_health_timeout(soak: Option<String>) -> Duration {
    let n = soak
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
        .min(50);
    Duration::from_secs(90 + 30 * n)
}

/// `n` bytes from the OS CSPRNG (`/dev/urandom`), hex-encoded. For keys that guard a vault; an
/// error, never a weaker fallback.
pub fn os_random_hex(n: usize) -> Result<String, String> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| format!("cannot read /dev/urandom for the node key: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn rand_hex(n: usize) -> String {
    let mut s = String::new();
    while s.len() < n {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        s.push_str(&format!("{:016x}", h.finish()));
    }
    s.truncate(n);
    s
}

/// The scenarios a selection names, or a usage error.
pub fn select<'a>(c: &'a Catalog, o: &RunOpts) -> Result<Vec<&'a Scenario>, String> {
    if o.firecracker && o.smoke {
        return Err("--firecracker and --smoke run different nodes; pass one".into());
    }
    if o.scenarios.is_empty() && o.tags.is_empty() && !o.all && !o.smoke && !o.firecracker {
        return Err("pass one of --scenarios, --tags, --all, --smoke or --firecracker".into());
    }
    for id in &o.scenarios {
        if !c.scenarios.iter().any(|(_, s)| &s.id == id) {
            return Err(format!("unknown scenario `{id}`"));
        }
    }
    for t in &o.tags {
        if !crate::scenario::TAGS.contains(&t.as_str()) {
            return Err(format!("unknown tag `{t}` (tags are areas)"));
        }
    }
    Ok(c.scenarios
        .iter()
        .map(|(_, s)| s)
        .filter(|s| {
            o.all
                || o.scenarios.contains(&s.id)
                || s.tags.iter().any(|t| o.tags.contains(t))
                || (o.smoke
                    && s.needs == Needs::Process
                    && s.status != Status::Retired
                    && !s.tags.iter().any(|t| t == "slow"))
                || (o.firecracker && s.needs == Needs::Firecracker && s.status != Status::Retired)
        })
        .collect())
}

/// The `--bench` scenarios: those with a `model` block that are not retired, narrowed by
/// `--scenarios` and `--tags` when given.
pub fn select_bench<'a>(c: &'a Catalog, o: &RunOpts) -> Result<Vec<&'a Scenario>, String> {
    for id in &o.scenarios {
        if !c.scenarios.iter().any(|(_, s)| &s.id == id) {
            return Err(format!("unknown scenario `{id}`"));
        }
    }
    let narrowed = !o.scenarios.is_empty() || !o.tags.is_empty();
    let v: Vec<&Scenario> = c
        .scenarios
        .iter()
        .map(|(_, s)| s)
        .filter(|s| s.model.is_some() && s.status != Status::Retired)
        .filter(|s| {
            !narrowed || o.scenarios.contains(&s.id) || s.tags.iter().any(|t| o.tags.contains(t))
        })
        .collect();
    if v.is_empty() {
        return Err("no scenario with a `model` block is selected".into());
    }
    Ok(v)
}

/// The models a bench run uses, after the gates that need no node (catalog, `--real`, the lane).
/// The node-side gates (`guestMode`, the credential) run once the node is up.
pub fn bench_models(b: &BenchOpts, firecracker: bool) -> Result<Vec<String>, String> {
    let models = if !b.models.is_empty() {
        b.models.clone()
    } else if b.mock {
        vec![crate::usage::MOCK_MODEL.to_string()]
    } else {
        return Err("--bench needs --mock or --model <catalog model>".into());
    };
    crate::bench::check_models(
        &models,
        b.real,
        &vam_inference::ModelCatalog::new(),
        firecracker,
        &b.provider,
    )?;
    Ok(models)
}

/// The task-set version of a selection: `--task-set-version`, else the one version every
/// selected scenario names. Refused unless pinned, and every task must be in the set.
pub fn bench_version(
    sets: &crate::bench::TaskSets,
    selected: &[&Scenario],
    want: Option<&str>,
) -> Result<String, String> {
    let mut versions: Vec<&str> = selected
        .iter()
        .filter_map(|s| s.model.as_ref().map(|m| m.version.as_str()))
        .collect();
    versions.sort();
    versions.dedup();
    let version = match (want, versions.as_slice()) {
        (Some(v), _) => v.to_string(),
        (None, [v]) => v.to_string(),
        (None, vs) => {
            return Err(format!(
                "task-set versions disagree across the selection ({}); pass --task-set-version",
                vs.join(", ")
            ))
        }
    };
    for s in selected {
        let m = s.model.as_ref().expect("selected for a model block");
        sets.check(&version, &m.task)?;
        if m.version != version {
            return Err(format!(
                "scenario `{}` is in task-set version `{}`, not `{version}`",
                s.id, m.version
            ));
        }
    }
    Ok(version)
}

/// `--bench --recorded DIR`: aggregates saved trials, starts nothing.
fn run_recorded(o: &RunOpts, b: &BenchOpts, dir: &std::path::Path) -> i32 {
    let sets_file = {
        let root = o.root.join(crate::bench::TASK_SETS_FILE);
        if root.is_file() {
            root
        } else {
            dir.join(crate::bench::TASK_SETS_FILE)
        }
    };
    let catalog = vam_inference::ModelCatalog::new();
    let res = crate::bench::TaskSets::load(&sets_file)
        .and_then(|sets| crate::bench::replay(dir, &sets, b.task_set_version.as_deref(), &catalog));
    let res = match res {
        Ok(r) => r,
        Err(e) => {
            eprintln!("vam-e2e run --bench: {e}");
            return 2;
        }
    };
    let out = o
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from("target/e2e").join(format!("bench-{}", rand_hex(6))));
    match crate::bench::write_results(&res, &out, std::path::Path::new(".")) {
        Ok(files) => {
            for f in files {
                println!("wrote {}", f.display());
            }
            println!(
                "bench (recorded, synthetic, not publishable): {} row(s), task-set {}",
                res["rows"].as_array().map(Vec::len).unwrap_or(0),
                res["taskSetVersion"].as_str().unwrap_or("")
            );
            0
        }
        Err(e) => {
            eprintln!("vam-e2e run --bench: {}: {e}", out.display());
            2
        }
    }
}

/// The vamd binary: `VAM_E2E_VAMD`, else `vamd` in the bin dir (`--bin-dir`, `VAM_E2E_BIN_DIR`,
/// `$CARGO_TARGET_DIR/debug`).
pub fn vamd_binary(bin_dir: &Option<PathBuf>) -> PathBuf {
    let bin_dir = bin_dir
        .clone()
        .or_else(|| std::env::var_os("VAM_E2E_BIN_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into()))
                .join("debug")
        });
    std::env::var_os("VAM_E2E_VAMD")
        .map(PathBuf::from)
        .unwrap_or_else(|| bin_dir.join("vamd"))
}

/// `vam-e2e node`: the bench node on a persistent state dir, left running so the owner can add
/// the provider key to its vault with `vam secrets set`. The runner never sees the key.
pub struct NodeOpts {
    pub state_dir: PathBuf,
    pub firecracker: bool,
    pub bin_dir: Option<PathBuf>,
}

pub fn node(o: NodeOpts) -> i32 {
    let (key, master_key) =
        match persistent_node(&Some(o.state_dir.clone()), |k| std::env::var(k).ok()) {
            Ok(Some(v)) => v,
            Ok(None) => unreachable!("a state dir was given"),
            Err(e) => {
                eprintln!("vam-e2e node: {e}");
                return 2;
            }
        };
    let extra_env = match node_env(o.firecracker) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("vam-e2e node: {e}");
            return 2;
        }
    };
    let vamd_bin = vamd_binary(&o.bin_dir);
    if !vamd_bin.is_file() {
        eprintln!(
            "vam-e2e node: {} not found. Build it first",
            vamd_bin.display()
        );
        return 2;
    }
    let dir = o.state_dir.join("node");
    let scripts = o.state_dir.join("mock-scripts");
    if let Err(e) = std::fs::create_dir_all(&scripts) {
        eprintln!("vam-e2e node: {}: {e}", scripts.display());
        return 2;
    }
    let mut vamd = match VamdProcess::new(VamdConfig {
        bin: vamd_bin,
        dir,
        scripts_dir: scripts,
        key: key.clone(),
        extra_env,
        snapshot_restore: o.firecracker,
        state_dir: Some(o.state_dir.clone()),
        master_key,
    }) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("vam-e2e node: {e}");
            return 2;
        }
    };
    if let Err(e) = vamd.start() {
        eprintln!("vam-e2e node: {e}");
        return 2;
    }
    let base = vamd.base_url();
    let ca = vamd.ca_path();
    let api = Api::with_ca_path(&base, &key, "node", ca.clone());
    let wait = if o.firecracker {
        lane_health_timeout(std::env::var("VAM_GOLDEN_BAKE_SOAK").ok())
    } else {
        Duration::from_secs(30)
    };
    let t = Instant::now();
    while !api.healthy() {
        if !vamd.alive() || t.elapsed() > wait {
            eprintln!(
                "vam-e2e node: vamd did not come up. Log tail:\n{}",
                vamd.log_tail(15)
            );
            vamd.stop();
            return 2;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let ca_path = ca.map(|p| p.display().to_string()).unwrap_or_default();
    let key_file = o.state_dir.join("node-key");
    println!("node up: {base}");
    println!("add the provider key (in another shell; the value is read from your own variable):");
    println!(
        "  VAM_CA_FILE={ca_path} vam secrets set anthropic/api-key --from-env ANTHROPIC_API_KEY \\\n    --scope org --host {base} --token \"$(cat {})\"",
        key_file.display()
    );
    println!(
        "then press Enter here to stop the node. The key stays in {}/vamd.db.",
        o.state_dir.display()
    );
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    while vamd.alive() && !stop.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(300));
    }
    let died = !vamd.alive();
    vamd.stop();
    if died {
        eprintln!(
            "vam-e2e node: vamd exited. Log tail:\n{}",
            vamd.log_tail(15)
        );
        return 1;
    }
    println!("node stopped");
    0
}

/// The base manifest a process node is given as `VAM_BASE_MANIFEST` (relative to the repo root,
/// where `scripts/e2e/run.sh` runs).
const BASE_MANIFEST_FIXTURE: &str = "e2e/world/fixtures/base/standard.base.json";

/// The environment the node needs on this lane (assets and diagnostics on the microVM lane).
pub fn node_env(firecracker: bool) -> Result<Vec<(String, String)>, String> {
    let mut extra_env = Vec::new();
    if firecracker {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        require_root_for_jailer(euid_from_status(&status))?;
        extra_env.extend(lane_assets()?);
        extra_env.extend(lane_diag_env_from(|k| std::env::var(k).ok()));
    } else {
        if let Ok(v) = std::env::var("VAMD_PROCESS_GUEST_BIN") {
            extra_env.push(("VAMD_PROCESS_GUEST_BIN".to_string(), v));
        }
        // A process node has no image, so it reports the base manifest it is given (a fixture).
        // The microVM lane reads the manifest beside its real image and never sets this.
        let manifest = std::path::Path::new(BASE_MANIFEST_FIXTURE);
        if let Ok(abs) = std::fs::canonicalize(manifest) {
            extra_env.push((
                "VAM_BASE_MANIFEST".to_string(),
                abs.to_string_lossy().into_owned(),
            ));
        }
    }
    Ok(extra_env)
}

/// For a persistent state dir: the node's admin key (kept in `<dir>/node-key`, mode 0600, made
/// on first use) and the vault master key, which must come from the environment and must not
/// be the built-in test constant. `None` without a state dir (a throwaway node).
pub fn persistent_node(
    state_dir: &Option<PathBuf>,
    get_env: impl Fn(&str) -> Option<String>,
) -> Result<Option<(String, Option<String>)>, String> {
    let Some(dir) = state_dir else {
        return Ok(None);
    };
    let master =
        crate::bench::persistent_master_key(get_env("VAMD_SECRETS_MASTER_KEY").as_deref())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join("node-key");
    let key = match std::fs::read_to_string(&path) {
        Ok(k) if !k.trim().is_empty() => k.trim().to_string(),
        _ => {
            let k = format!("vam_sk_e2e_{}", os_random_hex(32)?);
            write_private(&path, &k).map_err(|e| format!("{}: {e}", path.display()))?;
            k
        }
    };
    Ok(Some((key, Some(master))))
}

fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(text.as_bytes())
}

/// Exit code: 0 clean, 1 a scenario failed or was red for the wrong reason, 2 usage or IO.
pub fn run(o: RunOpts) -> i32 {
    if let Some(b) = &o.bench {
        if let Some(dir) = &b.recorded {
            return run_recorded(&o, b, dir);
        }
    }
    // A persistent node's master key is checked first, before anything is read, built or
    // created: a bad key is refused on its own.
    if o.state_dir.is_some() {
        if let Err(e) = crate::bench::persistent_master_key(
            std::env::var("VAMD_SECRETS_MASTER_KEY").ok().as_deref(),
        ) {
            eprintln!("vam-e2e run: {e}");
            return 2;
        }
    }
    let cat = Catalog::load(&o.root);
    let mut errors = cat.errors.clone();
    errors.extend(crate::check::check(&o.root));
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("{e}");
        }
        eprintln!("catalog has {} error(s); run `vam-e2e check`", errors.len());
        return 2;
    }
    let mut bench_plan: Option<(Vec<String>, String)> = None;
    let selected = match &o.bench {
        Some(b) => {
            let r = select_bench(&cat, &o).and_then(|sel| {
                if let Some(p) = &b.profile {
                    check_profile(p)?;
                }
                let models = bench_models(b, o.firecracker)?;
                let sets =
                    crate::bench::TaskSets::load(&o.root.join(crate::bench::TASK_SETS_FILE))?;
                let version = bench_version(&sets, &sel, b.task_set_version.as_deref())?;
                bench_plan = Some((models, version));
                Ok(sel)
            });
            match r {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("vam-e2e run --bench: {e}");
                    return 2;
                }
            }
        }
        None => match select(&cat, &o) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("vam-e2e run: {e}");
                return 2;
            }
        },
    };
    let tag = rand_hex(6);
    let out = o
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from("target/e2e").join(&tag));
    if let Err(e) = std::fs::create_dir_all(out.join("briefs")) {
        eprintln!("vam-e2e run: {}: {e}", out.display());
        return 2;
    }
    let out = std::fs::canonicalize(&out).unwrap_or(out);

    let vamd_bin = vamd_binary(&o.bin_dir);
    if !vamd_bin.is_file() {
        eprintln!(
            "vam-e2e run: {} not found. Build it first (scripts/e2e/run.sh does)",
            vamd_bin.display()
        );
        return 2;
    }

    let (run_list, skipped): (Vec<&Scenario>, Vec<&Scenario>) =
        selected.into_iter().partition(|s| {
            // Bench tasks are written for a process node; on the microVM lane they run on the
            // vm node (the mock and the real model are served by vamd either way).
            let lane_ok = s.needs == node_needs(o.firecracker)
                || (o.bench.is_some() && o.firecracker && s.model.is_some());
            lane_ok && s.status != Status::Retired
        });
    let skipped: Vec<(String, String)> = skipped
        .iter()
        .map(|s| {
            let why = if s.status == Status::Retired {
                "retired".to_string()
            } else {
                format!("needs {:?}", s.needs).to_lowercase()
            };
            println!("{:<20} {}  ({why})", "SKIP", s.id);
            (s.id.clone(), why)
        })
        .collect();

    let scripts = out.join("mock-scripts");
    if let Err(e) = crate::mock::write_scripts(&scripts, &run_list) {
        eprintln!("vam-e2e run: mock scripts: {e}");
        return 2;
    }
    let key = format!("vam_sk_e2e_{}", rand_hex(32));
    let (key, master_key) = match persistent_node(&o.state_dir, |k| std::env::var(k).ok()) {
        Ok(v) => v.unwrap_or((key, None)),
        Err(e) => {
            eprintln!("vam-e2e run: {e}");
            return 2;
        }
    };
    let mut extra_env = match node_env(o.firecracker) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("vam-e2e run: {e}");
            return 2;
        }
    };
    // The mock providers connector scenarios use start before vamd, so vamd can be told where
    // they are.
    let uses_mocks = run_list.iter().any(|s| {
        s.given.iter().any(|g| {
            cat.piece(g.piece())
                .is_some_and(|w| w.kind == crate::world::PieceKind::MockProvider)
        })
    });
    let connector_mocks = if uses_mocks {
        match crate::engine::connectors::Mocks::start() {
            Ok(m) => {
                extra_env.extend(crate::engine::connectors::node_env(&m));
                Some(m)
            }
            Err(e) => {
                eprintln!("vam-e2e run: mock providers: {e}");
                return 2;
            }
        }
    } else {
        None
    };
    // Marketplace repositories: the runner makes them under `<out>/marketplaces`; the node is told
    // which https URL stands for which repository only once vamd knows the seam key.
    let marketplace_root = out.join("marketplaces");
    if crate::engine::connectors::PASS_MARKETPLACE_SEAM {
        let used: Vec<crate::world::WorldPiece> = run_list
            .iter()
            .flat_map(|s| s.given.iter())
            .filter_map(|g| cat.piece(g.piece()).cloned())
            .filter(|w| w.kind == crate::world::PieceKind::GitMarketplace)
            .collect();
        if !used.is_empty() {
            extra_env.push((
                crate::engine::connectors::MARKETPLACE_SEAM_ENV.to_string(),
                crate::engine::marketplace::seam_json(used.iter(), &marketplace_root),
            ));
        }
    }
    let mut vamd = match VamdProcess::new(VamdConfig {
        bin: vamd_bin.clone(),
        dir: out.join("vamd"),
        scripts_dir: scripts,
        key: key.clone(),
        extra_env,
        snapshot_restore: o.firecracker,
        state_dir: o.state_dir.clone(),
        master_key,
    }) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("vam-e2e run: {e}");
            return 2;
        }
    };
    let spawn_err = vamd.start().err();
    let base = vamd.base_url();
    let ca_path = vamd.ca_path();
    let port = base.rsplit(':').next().unwrap_or("").to_string();
    let mut cfg = Cfg::new(RunTag(tag.clone()));
    let bench_real = o.bench.as_ref().is_some_and(|b| b.real);
    (cfg.step_budget, cfg.scenario_budget) =
        resolve_budgets(bench_real, o.step_budget, o.scenario_budget);
    let (step_budget_ms, scenario_budget_ms) = (
        cfg.step_budget.as_millis() as u64,
        cfg.scenario_budget.as_millis() as u64,
    );
    if o.firecracker {
        // The first start bakes the golden snapshot (and, with the lane's soak, N more)
        // before the API comes up.
        cfg.health_timeout = lane_health_timeout(std::env::var("VAM_GOLDEN_BAKE_SOAK").ok());
    }
    let pieces = cat.pieces.iter().map(|(_, w)| w.clone()).collect();
    let mut engine = Engine::new(
        Api::with_ca_path(&base, &key, &tag, ca_path),
        Box::new(vamd),
        cfg,
        pieces,
    );
    if let Some(m) = connector_mocks {
        engine.set_connector_mocks(m);
    }
    engine.set_marketplace_root(marketplace_root);
    if let Some(p) = o.bench.as_ref().and_then(|b| b.profile.as_deref()) {
        engine.set_profile(p);
    }
    if let Some((models, _)) = &bench_plan {
        engine.allow_models(models);
        if o.firecracker {
            engine.set_node_mode("vm");
        }
    }
    match spawn_err {
        Some(e) => {
            // The engine reports the failed start as class `env` in each scenario.
            engine.set_prepare_error(crate::engine::Fail::new(
                crate::scenario::Class::Env,
                "vamd starts",
                e,
            ));
        }
        None => engine.prepare(),
    }
    // The node-side gates for a real model: guestMode vm and the credential in the node's own
    // secret store. A refusal ends the invocation before any trial (nothing is spent).
    let bench_provider = o
        .bench
        .as_ref()
        .map(|b| b.provider.as_str())
        .unwrap_or(crate::bench::PROVIDER_DIRECT);
    if bench_provider == crate::bench::PROVIDER_VERCEL {
        engine.set_model_provider("vercel");
    }
    if let Some((models, _)) = &bench_plan {
        if engine.prepare_failure().is_none() {
            let gate = engine.node_facts().and_then(|(mode, ids)| {
                crate::bench::check_node(
                    mode.as_deref(),
                    &ids,
                    models,
                    &vam_inference::ModelCatalog::new(),
                    bench_provider,
                )
            });
            if let Err(e) = gate {
                engine.vamd.stop();
                eprintln!("vam-e2e run --bench: {e}");
                return 2;
            }
        }
    }

    let started = Instant::now();
    let started_at = format!("{:?}", std::time::SystemTime::now());
    let mut results: Vec<(&Scenario, ScenarioResult, Option<String>)> = Vec::new();
    // One entry per run: each scenario once, or k trials per model under `--bench`.
    let work: Vec<(&Scenario, Option<(String, u32)>)> = match (&o.bench, &bench_plan) {
        (Some(b), Some((models, _))) => run_list
            .iter()
            .flat_map(|s| {
                let k = b.k.or(s.model.as_ref().map(|m| m.k)).unwrap_or(1).max(1);
                models
                    .iter()
                    .flat_map(move |m| (1..=k).map(move |n| (*s, Some((m.clone(), n)))))
            })
            .collect(),
        _ => run_list.iter().map(|s| (*s, None)).collect(),
    };
    let cap = o.bench.as_ref().and_then(|b| b.max_usd_micro);
    let cap_catalog = vam_inference::ModelCatalog::new();
    let mut spent: i64 = 0;
    let mut stopped: Option<crate::bench::CapStop> = None;
    let mut trial_models: Vec<Option<String>> = Vec::new();
    let mut trial_ids: Vec<Option<(String, u32)>> = Vec::new();
    for (s, trial) in &work {
        let s: &&Scenario = s;
        let trial_model = trial.as_ref().map(|(m, _)| m.clone());
        trial_models.push(trial_model.clone());
        trial_ids.push(trial.clone());
        engine.set_trial(&s.id, trial.as_ref().map(|(m, n)| (m.as_str(), *n)));
        engine.set_model(trial_model.as_deref().unwrap_or(crate::usage::MOCK_MODEL));
        let res = if started.elapsed() > o.budget {
            let mut r = run_scenario_overbudget(s);
            r.report = crate::runner::report(s, &r.outcome);
            r
        } else {
            run_scenario(&mut engine, s)
        };
        println!("{}", crate::report::line(s, &res));
        let brief = if res.report != crate::runner::Report::Pass {
            let rel = match trial {
                Some((m, n)) => format!("briefs/{}.md", crate::report::trial_stem(&s.id, m, *n)),
                None => format!("briefs/{}.md", s.id),
            };
            let _ = std::fs::write(
                out.join(&rel),
                crate::report::brief(s, &res, std::path::Path::new(".")),
            );
            Some(rel)
        } else {
            None
        };
        if let Some((m, n)) = trial {
            let stem = crate::report::trial_stem(&s.id, m, *n);
            if let Err(e) = crate::report::write_trial(&out, &stem, m, *n, &res) {
                eprintln!("vam-e2e run --bench: trial folder {stem}: {e}");
            }
        }
        // The cap: sum the trial's cost as the rows will, and start no further trial past it.
        if let (Some(cap), Some(_)) = (cap, trial) {
            let c = crate::bench::cap_cost_micro(&res.measure(&cap_catalog));
            match crate::bench::cap_check(spent, c, cap) {
                Ok(total) => spent = total,
                Err(stop) => stopped = Some(stop),
            }
        }
        results.push((s, res, brief));
        if stopped.is_some() {
            break;
        }
    }
    engine.vamd.stop();
    if let Some(stop) = &stopped {
        eprintln!(
            "vam-e2e run --bench: stopped at the cost cap ({}): {} micro-USD spent, {} of {} trial(s) run",
            stop.reason,
            stop.spend_micro,
            results.len(),
            work.len()
        );
    }

    let exit = if stopped.is_some() {
        3
    } else if o.bench.is_some() {
        // A trial that does not pass is a measurement. Only a trial the harness could not run
        // (class harness or env) fails the invocation.
        let broken = results.iter().any(|(_, r, _)| {
            matches!(
                &r.outcome,
                crate::runner::Outcome::Failed { class, .. }
                    if matches!(class, crate::scenario::Class::Harness | crate::scenario::Class::Env)
            )
        });
        i32::from(broken)
    } else if results.iter().any(|(_, r, _)| r.report.fails_run()) {
        1
    } else {
        0
    };
    let rows: Vec<(&Scenario, &ScenarioResult, Option<String>)> =
        results.iter().map(|(s, r, b)| (*s, r, b.clone())).collect();
    let vamd_json = serde_json::json!({
        "binary": vamd_bin.display().to_string(),
        "port": port,
        "prepared": engine.prepare_failure().is_none(),
    });
    let summary = crate::report::summary(
        &tag,
        &started_at,
        started.elapsed().as_millis(),
        vamd_json,
        &rows,
        &skipped,
        exit,
        trial_ids
            .iter()
            .flatten()
            .all(|(m, _)| m == crate::usage::MOCK_MODEL),
        &trial_ids,
    );
    let _ = std::fs::write(
        out.join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap_or_default(),
    );
    if exit == 0 && !o.keep {
        let _ = std::fs::remove_dir_all(out.join("vamd").join("state"));
        let _ = std::fs::remove_dir_all(out.join("vamd").join("home"));
    }
    if let Some((models, version)) = &bench_plan {
        let catalog = vam_inference::ModelCatalog::new();
        let trials: Vec<crate::bench::Trial> = results
            .iter()
            .zip(&trial_models)
            .filter_map(|((s, r, _), model)| {
                let m = s.model.as_ref()?;
                Some(crate::bench::Trial {
                    task: m.task.clone(),
                    model: model.clone().unwrap_or_else(|| models[0].clone()),
                    mode: "on".into(),
                    pass: r.report == crate::runner::Report::Pass,
                    timed_out: r.timed_out,
                    measure: r.measure(&catalog),
                })
            })
            .collect();
        let k = o.bench.as_ref().and_then(|b| b.k).unwrap_or_else(|| {
            run_list
                .iter()
                .filter_map(|s| s.model.as_ref().map(|m| m.k))
                .max()
                .unwrap_or(1)
        });
        let res = crate::bench::results(
            &trials,
            &crate::bench::ResultsInput {
                task_set_version: version,
                k: u64::from(k),
                provenance: crate::bench::Provenance {
                    mock: models.iter().all(|m| m == crate::usage::MOCK_MODEL),
                    ..Default::default()
                },
                run_tag: &tag,
                step_budget_ms,
                scenario_budget_ms,
                asked: crate::bench::asked_harness(
                    o.bench.as_ref().and_then(|b| b.profile.as_deref()),
                ),
            },
            &catalog,
        );
        let mut res = res;
        if let (Some(stop), Some(cap)) = (&stopped, cap) {
            res["stoppedAt"] = serde_json::json!(stop.reason);
            res["maxUsd"] = serde_json::json!(cap as f64 / 1_000_000.0);
            res["spendMicro"] = serde_json::json!(stop.spend_micro);
        }
        match crate::bench::write_results(&res, &out, std::path::Path::new(".")) {
            Ok(files) => files.iter().for_each(|f| println!("wrote {}", f.display())),
            Err(e) => eprintln!("vam-e2e run --bench: results: {e}"),
        }
    }
    let t = &summary["totals"];
    println!(
        "{} scenario(s): {} pass, {} fail, {} red (right reason), {} red (wrong reason), {} skipped. Output: {}",
        results.len(), t["pass"], t["fail"], t["redRightReason"], t["redWrongReason"], t["skipped"], out.display()
    );
    exit
}

fn run_scenario_overbudget(s: &Scenario) -> ScenarioResult {
    use crate::runner::{Outcome, Report};
    ScenarioResult {
        outcome: Outcome::Failed {
            position: "when.0".into(),
            class: crate::scenario::Class::Harness,
        },
        report: Report::Fail,
        timed_out: false,
        expected: "the invocation budget has time left".into(),
        observed: format!("invocation budget used up before `{}` started", s.id),
        duration_ms: 0,
        evidence: crate::driver::Evidence {
            runs: vec![],
            refusal: None,
            vamd_log: String::new(),
            guest_logs: vec![],
            extras: vec![],
            ledger: vec![],
        },
    }
}

/// Step and scenario budgets in force. Mock, smoke and CI keep 30 s / 120 s; a real model under
/// `--bench` gets 180 s / 10 min (each trial is its own scenario run, so the scenario budget is
/// per trial). A flag overrides either default on any run.
pub fn resolve_budgets(
    bench_real: bool,
    step: Option<Duration>,
    scenario: Option<Duration>,
) -> (Duration, Duration) {
    let (s, sc) = if bench_real {
        (Duration::from_secs(180), Duration::from_secs(600))
    } else {
        (Duration::from_secs(30), Duration::from_secs(120))
    };
    (step.unwrap_or(s), scenario.unwrap_or(sc))
}

pub fn parse_budget(s: &str) -> Result<Duration, String> {
    parse_duration(s).ok_or_else(|| format!("bad duration `{s}` (use 300ms, 45s or 10m)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_defaults_and_overrides() {
        let (s, sc) = (Duration::from_secs, Duration::from_secs);
        assert_eq!(resolve_budgets(false, None, None), (s(30), sc(120)));
        assert_eq!(resolve_budgets(true, None, None), (s(180), sc(600)));
        assert_eq!(resolve_budgets(false, Some(s(5)), None), (s(5), sc(120)));
        assert_eq!(
            resolve_budgets(true, Some(s(60)), Some(sc(300))),
            (s(60), sc(300))
        );
    }

    #[test]
    fn os_random_hex_is_long_hex_and_differs() {
        let (a, b) = (os_random_hex(32).unwrap(), os_random_hex(32).unwrap());
        assert_ne!(a, b);
        for k in [&a, &b] {
            assert_eq!(k.len(), 64);
            assert!(k
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        }
    }

    #[test]
    fn the_persistent_admin_key_is_private_prefixed_and_random() {
        use std::os::unix::fs::PermissionsExt;
        let t = std::env::temp_dir().join(format!("vam-e2e-nodekey-{}", rand_hex(8)));
        let a = persistent_node(&Some(t.join("a")), |_| Some("cd".repeat(32)))
            .unwrap()
            .unwrap()
            .0;
        let b = persistent_node(&Some(t.join("b")), |_| Some("cd".repeat(32)))
            .unwrap()
            .unwrap()
            .0;
        assert_ne!(a, b);
        let hex = a.strip_prefix("vam_sk_e2e_").expect("prefix");
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        let mode = std::fs::metadata(t.join("a/node-key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn lane_diag_env_is_forwarded_only_when_set() {
        assert!(lane_diag_env_from(|_| None).is_empty());
        assert!(lane_diag_env_from(|_| Some(String::new())).is_empty());
        let got = lane_diag_env_from(|k| (k == "VAM_SERIAL_CONSOLE").then(|| "1".to_string()));
        assert_eq!(
            got,
            vec![("VAM_SERIAL_CONSOLE".to_string(), "1".to_string())]
        );
        let all = lane_diag_env_from(|k| k.starts_with("VAM_").then(|| "1".to_string()));
        assert_eq!(all.len(), 4);
        for k in ["VAM_TIMING", "VAM_GOLDEN_BAKE_SOAK"] {
            assert!(all.iter().any(|(n, _)| n == k), "{k} must be forwarded");
        }
        // Nothing else leaks through.
        assert!(lane_diag_env_from(|k| (k == "HOME").then(|| "/x".to_string())).is_empty());
    }

    #[test]
    fn lane_health_timeout_grows_with_the_soak() {
        assert_eq!(lane_health_timeout(None), Duration::from_secs(90));
        assert_eq!(
            lane_health_timeout(Some("x".into())),
            Duration::from_secs(90)
        );
        assert_eq!(
            lane_health_timeout(Some("5".into())),
            Duration::from_secs(240)
        );
    }

    #[test]
    fn the_microvm_lane_needs_root_for_the_jailer() {
        assert_eq!(
            euid_from_status("Name:\tx\nUid:\t1001\t0\t1001\t1001\n"),
            Some(0)
        );
        assert_eq!(
            euid_from_status("Uid:\t1001\t1001\t1001\t1001\n"),
            Some(1001)
        );
        assert_eq!(euid_from_status("Name:\tx\n"), None);
        assert!(require_root_for_jailer(Some(0)).is_ok());
        let e = require_root_for_jailer(Some(1001)).unwrap_err();
        assert!(
            e.contains("needs root") && e.contains("sudo") && e.contains("class env"),
            "{e}"
        );
        assert!(require_root_for_jailer(None).is_err());
    }

    #[test]
    fn lane_assets_need_all_four_files() {
        let t = std::env::temp_dir().join(format!("vam-e2e-assets-{}", rand_hex(8)));
        std::fs::create_dir_all(&t).unwrap();
        let f = t.join("x");
        std::fs::write(&f, "x").unwrap();
        let p = f.display().to_string();
        let env = |missing: &'static str| {
            let p = p.clone();
            move |k: &str| {
                if k == missing {
                    None
                } else {
                    Some(p.clone())
                }
            }
        };
        let ok = lane_assets_from(env("")).unwrap();
        assert_eq!(ok.len(), 4);
        let twin = lane_assets_from(|k| {
            (k == "VAM_FIRECRACKER_BIN"
                || k.starts_with("VAM_") && !k.contains("BIN")
                || k == "VAMD_JAILER_BIN")
                .then(|| p.clone())
        })
        .unwrap();
        assert_eq!(twin[0].0, "VAMD_FIRECRACKER_BIN");
        assert!(lane_assets_from(env("VAM_KERNEL_PATH"))
            .unwrap_err()
            .contains("VAM_KERNEL_PATH"));
        assert!(lane_assets_from(|_| Some("/nonexistent/x".into()))
            .unwrap_err()
            .contains("not a file"));
        let _ = std::fs::remove_dir_all(t);
    }
}
