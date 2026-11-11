use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "vam-e2e", about = "VAM E2E scenarios")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

// One-shot CLI value parsed once; boxing would only add noise.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Cmd {
    /// Validate the scenario and world-piece catalog. Runs nothing. Exit 0 clean, 1 errors, 2 usage or IO.
    Check {
        /// The catalog root (holds `scenarios/` and `world/`).
        #[arg(long, default_value = "e2e")]
        root: PathBuf,
        /// The docs pages to lint (status field, claim proofs). Default `docs/src/content/docs`,
        /// skipped when absent; an explicit path that is absent exits 2.
        #[arg(long)]
        docs: Option<PathBuf>,
        /// The README to lint. Default `<root>/../README.md`, skipped when absent.
        #[arg(long)]
        readme: Option<PathBuf>,
    },
    /// Run scenarios against a throwaway vamd. Exit 0 clean, 1 a scenario failed or was red for
    /// the wrong reason, 2 usage or IO.
    Run {
        /// The catalog root (holds `scenarios/` and `world/`).
        #[arg(long, default_value = "e2e")]
        root: PathBuf,
        #[arg(long, value_delimiter = ',')]
        scenarios: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        #[arg(long)]
        all: bool,
        /// Every non-retired `needs: process` scenario.
        #[arg(long)]
        smoke: bool,
        /// Every non-retired `needs: firecracker` scenario, on a real microVM node. Assets come
        /// from VAMD_FIRECRACKER_BIN, VAMD_JAILER_BIN, VAM_KERNEL_PATH and VAM_ROOTFS_PATH.
        #[arg(long)]
        firecracker: bool,
        /// Whole-invocation budget (300ms, 45s, 15m). Default 15m.
        #[arg(long, default_value = "15m")]
        budget: String,
        /// Step budget (300ms, 45s, 3m). Default 30s; 180s with --bench --real.
        #[arg(long)]
        step_budget: Option<String>,
        /// Scenario budget (per trial under --bench). Default 120s; 10m with --bench --real.
        #[arg(long)]
        scenario_budget: Option<String>,
        /// Output directory (default target/e2e/<runTag>).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Where vamd is (default $CARGO_TARGET_DIR/debug or target/debug).
        #[arg(long)]
        bin_dir: Option<PathBuf>,
        /// Keep the vamd state dirs after a clean run.
        #[arg(long)]
        keep: bool,
        /// Run on a persistent node state dir (the vault holds the provider key; see
        /// `vam-e2e node`). Needs VAMD_SECRETS_MASTER_KEY in the environment.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Run the scenarios that carry a `model` block k times each and write
        /// `bench-results.json` (pass^k, pass@k, cost per success, ...).
        #[arg(long)]
        bench: bool,
        /// With --bench: use mock-model and the scenarios' `mock` blocks. Never publishable.
        #[arg(long)]
        mock: bool,
        /// With --bench: allow a model other than mock-model (it spends money).
        #[arg(long)]
        real: bool,
        /// With --bench: catalog models to run (repeat or comma-separate).
        #[arg(long = "model", value_delimiter = ',')]
        models: Vec<String>,
        /// With --bench: `direct` (the model maker's endpoint, default) or `vercel` (the Vercel AI
        /// Gateway, transport only: cost is still the maker's list price).
        #[arg(long)]
        provider: Option<String>,
        /// With --bench: aggregate recorded trials from this directory (`trials.json`); starts nothing.
        #[arg(long)]
        recorded: Option<PathBuf>,
        /// With --bench: require this pinned task-set version.
        #[arg(long)]
        task_set_version: Option<String>,
        /// With --bench: trials per (task, model), over the scenario's `model.k`.
        #[arg(short = 'k')]
        k: Option<u32>,
        /// With --bench: the harness profile every trial names (`inputs.harness.profile`).
        /// Without it no profile is sent, which is `base`. An unknown name is refused before
        /// any trial starts.
        #[arg(long)]
        profile: Option<String>,
        /// With --bench: stop starting trials once the run's cost (the maker's list price, as in
        /// the rows) passes this many USD, write what there is, and exit non-zero. A trial whose
        /// cost is unknown stops the run too. A running trial is not killed.
        #[arg(long)]
        max_usd: Option<f64>,
    },
    /// Merge one run's `bench-results.json` into `bench/results/<version>.json` (by task, model,
    /// mode and profile; other rows stay as they are). Refuses a mock, recorded, synthetic or
    /// cost-capped result. Run it on the Mac in a writable checkout, once per run directory.
    BenchMerge {
        /// The run's `bench-results.json`.
        file: PathBuf,
        /// The published file (default `bench/results/<taskSetVersion>.json`).
        #[arg(long)]
        into: Option<PathBuf>,
    },
    /// Start the bench node on a persistent state dir and leave it running, so the provider key
    /// can be added to its vault (`vam secrets set`). Needs VAMD_SECRETS_MASTER_KEY.
    Node {
        /// The state dir: the vault (`vamd.db`) and the node's admin key (`node-key`).
        #[arg(long)]
        state_dir: PathBuf,
        /// The microVM node (needs root and the lane assets), as for `run --firecracker`.
        #[arg(long)]
        firecracker: bool,
        /// Where vamd is (default $CARGO_TARGET_DIR/debug or target/debug).
        #[arg(long)]
        bin_dir: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().cmd {
        Cmd::Check { root, docs, readme } => {
            if !root.join("scenarios").is_dir() {
                eprintln!(
                    "{}: io: no `scenarios` directory (run from the repo root or pass --root)",
                    root.display()
                );
                return ExitCode::from(2);
            }
            let mut errors = vam_e2e::check::check(&root);
            for (what, p) in [("--docs", &docs), ("--readme", &readme)] {
                if let Some(p) = p {
                    if !p.exists() {
                        eprintln!("{}: io: {what} path does not exist", p.display());
                        return ExitCode::from(2);
                    }
                }
            }
            let docs_dir = docs
                .clone()
                .unwrap_or_else(|| PathBuf::from("docs/src/content/docs"));
            let readme_file = readme.clone().unwrap_or_else(|| root.join("../README.md"));
            let docs_dir = (docs.is_some() || docs_dir.is_dir()).then_some(docs_dir);
            let readme_file = (readme.is_some() || readme_file.is_file()).then_some(readme_file);
            if docs_dir.is_some() || readme_file.is_some() {
                errors.extend(vam_e2e::docs::check_docs(
                    &root,
                    docs_dir.as_deref().unwrap_or(std::path::Path::new("")),
                    readme_file.as_deref(),
                ));
            }
            if errors.is_empty() {
                let c = vam_e2e::catalog::Catalog::load(&root);
                let features = vam_e2e::docs::feature_counts(&root)
                    .map(|(n, proven, dev)| {
                        format!(", features: {n}, proven {proven}, with a developer page {dev}")
                    })
                    .unwrap_or_default();
                println!(
                    "ok: {} scenarios, {} world pieces{features}",
                    c.scenarios.len(),
                    c.pieces.len()
                );
                ExitCode::SUCCESS
            } else {
                for e in &errors {
                    println!("{e}");
                }
                println!("{} error(s)", errors.len());
                ExitCode::from(1)
            }
        }
        Cmd::BenchMerge { file, into } => {
            match vam_e2e::bench::merge_file(&file, into.as_deref(), std::path::Path::new(".")) {
                Ok(lines) => {
                    lines.iter().for_each(|l| println!("{l}"));
                    println!("{} row(s) merged", lines.len());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("vam-e2e bench-merge: {e}");
                    ExitCode::from(2)
                }
            }
        }
        Cmd::Node {
            state_dir,
            firecracker,
            bin_dir,
        } => ExitCode::from(vam_e2e::run_cmd::node(vam_e2e::run_cmd::NodeOpts {
            state_dir,
            firecracker,
            bin_dir,
        }) as u8),
        Cmd::Run {
            root,
            scenarios,
            tags,
            all,
            smoke,
            firecracker,
            budget,
            step_budget,
            scenario_budget,
            out,
            bin_dir,
            keep,
            state_dir,
            bench,
            mock,
            real,
            models,
            provider,
            recorded,
            task_set_version,
            k,
            profile,
            max_usd,
        } => {
            let flags = mock
                || real
                || provider.is_some()
                || !models.is_empty()
                || recorded.is_some()
                || k.is_some()
                || task_set_version.is_some()
                || profile.is_some()
                || max_usd.is_some();
            if flags && !bench {
                eprintln!("vam-e2e run: --mock, --real, --model, --provider, --recorded, --task-set-version, --profile, --max-usd and -k need --bench");
                return ExitCode::from(2);
            }
            let max_usd_micro = match max_usd.map(vam_e2e::bench::parse_max_usd).transpose() {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("vam-e2e run: {e}");
                    return ExitCode::from(2);
                }
            };
            let bench = bench.then(|| vam_e2e::run_cmd::BenchOpts {
                mock,
                real,
                models,
                provider: provider.unwrap_or_else(|| vam_e2e::bench::PROVIDER_DIRECT.to_string()),
                recorded,
                task_set_version,
                k,
                profile,
                max_usd_micro,
            });
            let budget = match vam_e2e::run_cmd::parse_budget(&budget) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("vam-e2e run: {e}");
                    return ExitCode::from(2);
                }
            };
            let opt = |v: Option<String>| -> Result<Option<std::time::Duration>, String> {
                v.map(|s| vam_e2e::run_cmd::parse_budget(&s)).transpose()
            };
            let (step_budget, scenario_budget) = match (opt(step_budget), opt(scenario_budget)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => {
                    eprintln!("vam-e2e run: {e}");
                    return ExitCode::from(2);
                }
            };
            let code = vam_e2e::run_cmd::run(vam_e2e::run_cmd::RunOpts {
                root,
                scenarios,
                tags,
                all,
                smoke,
                firecracker,
                budget,
                step_budget,
                scenario_budget,
                out,
                bin_dir,
                keep,
                state_dir,
                bench,
            });
            ExitCode::from(code as u8)
        }
    }
}
