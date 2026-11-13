#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use vam_core::CidAllocator;
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::telemetry_stream::serve_telemetry_grpc;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::daemon::DaemonConfig;
use vam_orchestrator::manager::{
    drive9_layout, drive9_runtime, net_pool_for_runtime, run_manager_loop, sweep_active_duration,
    sweep_expired_approvals, sweep_keep_warm, SessionManager,
};
use vam_orchestrator::reconcile::run_startup_reconciliation;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;
use vam_telemetry::{RetentionManager, TenantSpoolManager};

/// A vamd built without `process-guest` has no process mode. Any `VAMD_PROCESS_*`
/// variable means someone expects it, so refuse instead of silently running VMs.
#[cfg(not(feature = "process-guest"))]
fn reject_process_env() {
    if let Some(name) = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .find(|k| k.starts_with("VAMD_PROCESS_"))
    {
        eprintln!(
            "vamd: {name} is set, but process mode is not compiled into this vamd (a test-only build option)"
        );
        std::process::exit(1);
    }
}

/// Startup failure: print one line naming what failed and exit 1, instead of a panic backtrace.
trait OrExit<T> {
    fn or_exit(self, what: &str) -> T;
}

impl<T, E: std::fmt::Display> OrExit<T> for Result<T, E> {
    fn or_exit(self, what: &str) -> T {
        match self {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("{what}: {e}");
                eprintln!("vamd: {what}: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn state_dir_of(config: &DaemonConfig) -> std::path::PathBuf {
    config
        .db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// `vamd --preflight`: print the start rules as JSON. Exit 0 when every check is ok or
/// skipped, 1 otherwise. Starts nothing and writes nothing.
fn run_preflight(config: &DaemonConfig) -> ! {
    let report = vam_orchestrator::preflight::run(
        config,
        |k| std::env::var(k).ok(),
        config.preflight_root.as_deref(),
    );
    let failed = report
        .checks
        .iter()
        .any(|c| c.status != "ok" && c.status != "skipped");
    match serde_json::to_string(&report) {
        Ok(json) => println!("{json}"),
        Err(e) => {
            eprintln!("vamd: cannot print the preflight report: {e}");
            std::process::exit(1);
        }
    }
    std::process::exit(i32::from(failed));
}

/// `vamd --init`: the state dir is created 0700 when it does not exist (an existing one is
/// left as the operator made it).
fn prepare_state_dir(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if dir.exists() {
        return;
    }
    let made = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)));
    if let Err(e) = made {
        eprintln!("vamd: cannot create the state dir {}: {e}", dir.display());
        std::process::exit(1);
    }
}

/// `vamd --init`, after the TLS material exists: the DB, the signing keys, then the first
/// admin key when the node has none. The secret goes to stdout once and nowhere else: not to
/// tracing, not to an error, not to a file (the DB keeps its sha256).
fn run_init(config: &DaemonConfig) -> ! {
    let state_dir = state_dir_of(config);
    let store = match SqliteWorkflowStore::open(&config.db_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "vamd: cannot open the database {}: {e}",
                config.db_path.display()
            );
            std::process::exit(1);
        }
    };
    match vam_orchestrator::signing::NodeKeys::load_or_create(&state_dir) {
        Ok(keys) => eprintln!("vamd: {}", keys.startup_line()),
        Err(e) => {
            eprintln!("vamd: cannot load the node signing keys: {e}");
            std::process::exit(1);
        }
    }
    match vam_orchestrator::first_key::init_first_key(&store, config.bootstrap_api_key.clone()) {
        Ok(vam_orchestrator::first_key::InitKey::Created(secret)) => {
            println!("admin key: {secret}");
            println!("save it now; it is not shown again");
        }
        Ok(vam_orchestrator::first_key::InitKey::Existing) => {
            println!("this node already has an admin key; none shown");
        }
        Err(e) => {
            eprintln!("vamd: cannot create the first admin key: {e}");
            std::process::exit(1);
        }
    }
    std::process::exit(0);
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().collect();
    if let Some(msg) = vam_orchestrator::daemon::one_shot_flag_error(&args) {
        eprintln!("vamd: {msg}");
        std::process::exit(2);
    }
    let config = DaemonConfig::parse();
    if config.preflight_oneshot {
        run_preflight(&config);
    }
    if config.init_oneshot {
        prepare_state_dir(&state_dir_of(&config));
    }
    // Process-guest mode (test nodes only): checked before any state is opened.
    #[cfg(not(feature = "process-guest"))]
    reject_process_env();
    #[cfg(feature = "process-guest")]
    let process_mode = {
        let state_root = config
            .db_path
            .parent()
            .map(|p| p.join("process-guest"))
            .unwrap_or_else(|| std::path::PathBuf::from("process-guest"));
        match vam_orchestrator::process_mode::validate_env(
            config.listen_addr,
            config.telemetry_listen_addr,
            state_root,
        ) {
            Ok(mode) => mode,
            Err(refusal) => {
                eprintln!("{refusal}");
                std::process::exit(1);
            }
        }
    };
    // How the API is served (TLS by default). A bad or half-set certificate pair stops
    // vamd here, before any state is opened; it never falls back to plain HTTP.
    let transport = {
        let state_dir = config
            .db_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let settings = vam_orchestrator::tls::TlsSettings {
            cert: config.tls_cert_path.as_deref(),
            key: config.tls_key_path.as_deref(),
            insecure_dev: config.insecure_dev,
            extra_sans: &config.tls_extra_sans,
            listen: config.listen_addr,
            state_dir: &state_dir,
        };
        match vam_orchestrator::tls::resolve(&settings) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("vamd: cannot serve the API: {e}");
                std::process::exit(1);
            }
        }
    };
    if config.init_oneshot {
        run_init(&config);
    }
    // The node's signing keys (receipts, attestations, per-session token keys), created on
    // first start under `<state dir>/signing/`. A key file that is loose or unreadable stops
    // vamd here; there is no fallback key. Only key ids are printed.
    {
        let state_dir = config
            .db_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        match vam_orchestrator::signing::NodeKeys::load_or_create(&state_dir) {
            Ok(keys) => {
                eprintln!("vamd: {}", keys.startup_line());
                vam_orchestrator::signing::install(keys);
            }
            Err(e) => {
                eprintln!("vamd: cannot load the node signing keys: {e}");
                std::process::exit(1);
            }
        }
    }
    let tenant_id = match config.tenant() {
        Ok(id) => id,
        Err(e) => {
            eprintln!("invalid VAMD_TENANT_ID: {e}");
            std::process::exit(1);
        }
    };
    let registry_domains = match config.registry_domains() {
        Ok(domains) => domains,
        Err(e) => {
            eprintln!("vamd: invalid VAMD_REGISTRY_DOMAIN / VAMD_MARKETPLACE_DOMAIN: {e}");
            std::process::exit(1);
        }
    };
    let registry_domains_log = registry_domains.clone();
    let tenant_ctx = vam_telemetry::TenantContext {
        tenant_id: tenant_id.clone(),
        retention_policy: config.retention_policy.clone(),
        spool_root: config.spool_dir.clone(),
    };
    vam_telemetry::install_tenant_context(tenant_ctx.clone());
    let tenant_spool = TenantSpoolManager::new(&config.spool_dir);
    if let Err(e) = tenant_spool.get_or_create_spool(&tenant_id, 64 * 1024 * 1024) {
        eprintln!("failed to create tenant spool: {e}");
        std::process::exit(1);
    }
    let tenant_spool_dir = tenant_ctx.tenant_spool_dir();
    let _ = std::fs::create_dir_all(tenant_ctx.tenant_audit_dir());
    let retention = std::sync::Arc::new(RetentionManager::new(config.retention_policy.clone()));
    vam_telemetry::install_retention_manager(retention.clone());

    vam_telemetry::init_telemetry_with_otlp("vamd", &config.otlp_config()).ok();

    tracing::info!("=== Phase 1: Config Parsing ===");
    tracing::info!("Config: {:?}", config.redacted());
    #[cfg(feature = "process-guest")]
    if let Some(mode) = &process_mode {
        tracing::warn!("{}", vam_orchestrator::process_mode::banner(mode));
        vam_orchestrator::process_mode::install(mode.clone());
    }
    vam_orchestrator::brokers::install_egress_passthrough_hosts(
        config.egress_passthrough_hosts.clone(),
    );
    vam_orchestrator::brokers::install_egress_max_bytes(config.egress_max_bytes);
    vam_orchestrator::registry::install_registry_domains(registry_domains);
    tracing::info!(
        registry_domain = registry_domains_log.registry_domain(),
        marketplace_domain = registry_domains_log.marketplace_domain(),
        "vamd registry domains"
    );
    tracing::info!(
        egress_max_bytes = config.egress_max_bytes,
        "vamd per-session egress byte cap (0 = unlimited)"
    );
    // Scripted mock model for the E2E journeys (test only). Off unless
    // VAMD_E2E_MOCK_SCRIPTS names a directory; a bad directory stops vamd here.
    let mock_scripts_dir = std::env::var("VAMD_E2E_MOCK_SCRIPTS").ok();
    let mock_scripts =
        match vam_orchestrator::brokers::load_mock_scripts(mock_scripts_dir.as_deref()) {
            Ok(scripts) => scripts,
            Err(err) => {
                eprintln!("vamd: VAMD_E2E_MOCK_SCRIPTS could not be loaded: {err}");
                std::process::exit(1);
            }
        };
    if let Some(scripts) = &mock_scripts {
        tracing::warn!(
            "e2e mock scripts: {} loaded from {}; this node is for tests only",
            scripts.ids().count(),
            mock_scripts_dir.as_deref().unwrap_or_default()
        );
    }
    vam_orchestrator::brokers::install_mock_scripts(mock_scripts.clone());
    // A test node (mock scripts loaded, process-guest build) also serves the fake HTTPS
    // origin the egress scenarios use. Neither exists in a default build.
    #[cfg(feature = "process-guest")]
    if mock_scripts.is_some() {
        if let Err(err) = vam_orchestrator::brokers::install_egress_fixture().await {
            eprintln!("vamd: the e2e egress fixture could not start: {err}");
            std::process::exit(1);
        }
    }
    tracing::info!(
        tenant_id = tenant_id.as_str(),
        tenant_spool = %tenant_spool_dir.display(),
        "vamd tenant spool isolation"
    );
    tracing::info!(
        otlp_enabled = config.otlp_enabled,
        otlp_endpoint = %config.otlp_endpoint,
        otlp_disabled_reason = config.otlp_disabled_reason.as_deref().unwrap_or(""),
        "vamd OTLP export (fail-open if collector is down)"
    );

    tracing::info!("=== Phase 2: Reconciliation ===");
    let db_path = &config.db_path;
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let reset_password_user = config.reset_password_user.clone();
    let bootstrap_admin = config.bootstrap_admin.clone();
    let bootstrap_admin_oneshot = config.bootstrap_admin_oneshot;
    let store = match SqliteWorkflowStore::open(db_path) {
        Ok(store) => Arc::new(store),
        Err(e) if reset_password_user.is_some() || bootstrap_admin_oneshot => {
            eprintln!("Failed to open DB at {:?}: {e}", db_path);
            std::process::exit(1);
        }
        Err(e) => {
            tracing::warn!(
                "Failed to open DB at {:?}: {}. Falling back to in-memory store.",
                db_path,
                e
            );
            Arc::new(SqliteWorkflowStore::in_memory().or_exit("cannot open the in-memory database"))
        }
    };

    if let Err(e) = store.set_tool_hitl_timeout_secs(config.tool_hitl_timeout_secs) {
        tracing::warn!(
            "tool HITL timeout {}s was not stored ({e}); new approvals keep the operator default of {}s",
            config.tool_hitl_timeout_secs,
            vam_orchestrator::allowed_models::DEFAULT_TOOL_HITL_TIMEOUT_SECS
        );
    }

    if let Some(target) = reset_password_user {
        let new_password = match std::env::var(vam_orchestrator::RESET_PASSWORD_ENV) {
            Ok(value) if !value.is_empty() => value,
            _ => {
                eprintln!(
                    "--reset-password requires {} to be set (value is never logged)",
                    vam_orchestrator::RESET_PASSWORD_ENV
                );
                std::process::exit(1);
            }
        };
        match vam_orchestrator::password::apply_admin_password_reset(&store, &target, &new_password)
        {
            Ok(user) => {
                println!(
                    "Password reset for user '{}' (username '{}')",
                    user.user_id, user.username
                );
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("password reset failed: {e}");
                std::process::exit(1);
            }
        }
    }

    let bootstrap_admin_password =
        std::env::var(vam_orchestrator::BOOTSTRAP_ADMIN_PASSWORD_ENV).ok();
    let bootstrap_admin_username = bootstrap_admin.clone().or_else(|| {
        bootstrap_admin_password
            .as_ref()
            .filter(|p| !p.is_empty())
            .map(|_| vam_orchestrator::OPERATOR_USERNAME.to_string())
    });
    if bootstrap_admin_oneshot {
        let username = match bootstrap_admin_username.clone() {
            Some(name) if !name.is_empty() => name,
            _ => {
                eprintln!(
                    "--bootstrap-admin requires a username argument or {}",
                    vam_orchestrator::BOOTSTRAP_ADMIN_ENV
                );
                std::process::exit(1);
            }
        };
        let password = match bootstrap_admin_password.clone() {
            Some(value) if !value.is_empty() => value,
            _ => {
                eprintln!(
                    "--bootstrap-admin requires {} to be set (value is never logged)",
                    vam_orchestrator::BOOTSTRAP_ADMIN_PASSWORD_ENV
                );
                std::process::exit(1);
            }
        };
        match vam_orchestrator::apply_headless_admin_bootstrap(&store, &username, &password) {
            Ok(user) => {
                println!(
                    "Bootstrapped admin user '{}' (username '{}')",
                    user.user_id, user.username
                );
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("admin bootstrap failed: {e}");
                std::process::exit(1);
            }
        }
    } else if let (Some(username), Some(password)) = (
        bootstrap_admin_username.filter(|s| !s.is_empty()),
        bootstrap_admin_password.filter(|s| !s.is_empty()),
    ) {
        match vam_orchestrator::apply_headless_admin_bootstrap(&store, &username, &password) {
            Ok(user) => {
                tracing::info!(
                    user_id = user.user_id.as_str(),
                    username = user.username.as_str(),
                    "headless first-admin bootstrap applied"
                );
            }
            Err(vam_orchestrator::OrchestratorError::Conflict(msg)) => {
                tracing::info!("headless first-admin bootstrap skipped: {msg}");
            }
            Err(e) => {
                eprintln!("admin bootstrap failed: {e}");
                std::process::exit(1);
            }
        }
    }

    let event_bus = Arc::new(vam_telemetry::BroadcastEventBus::new());
    store.set_event_bus(event_bus.clone());

    let audit_ledger = Arc::new(vam_telemetry::PartitionedAuditLedger::new());
    store.set_audit_ledger(audit_ledger);
    tracing::info!(
        tenant_id = tenant_id.as_str(),
        audit_dir = %tenant_ctx.tenant_audit_dir().display(),
        "Partitioned audit ledger attached (in-memory; JSONL under {{spool}}/{{tenant}}/audit)"
    );

    let mut egress_config = config.egress_config();
    egress_config.spool_dir = tenant_spool_dir.clone();
    vam_orchestrator::egress::log_egress_startup(&egress_config);
    let standards_egress = Arc::new(vam_orchestrator::egress::StandardsEgress::new(
        egress_config,
    ));
    vam_orchestrator::egress::install_standards_egress(standards_egress.clone());
    let egress_store = store.clone();
    let egress_bus = event_bus.clone();
    let egress_handle = tokio::spawn(vam_orchestrator::egress::run_standards_egress(
        egress_store,
        egress_bus,
        standards_egress,
    ));

    let vault = Arc::new(
        SecretsVault::initialize(store.clone(), db_path).or_exit("secrets vault init failed"),
    );
    vault
        .import_env_secrets_if_enabled()
        .or_exit("importing secrets from the environment failed");

    let cid_allocator = Arc::new(CidAllocator::default());

    let drive9_root = std::env::var("VAM_DRIVE9_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/var/lib/vam/drive9"));
    // The jailer: on by default on a microVM node. A node that cannot jail refuses to
    // start unless the operator opted out (`--no-jailer` / `VAMD_USE_JAILER=0`).
    let jail_cfg = vam_core::JailConfig {
        uid: config.jailer_uid,
        gid: config.jailer_gid,
        cgroup_version: config.jailer_cgroup_version,
        ..Default::default()
    };
    let vm_node = vam_orchestrator::guest_mode() == "vm";
    if vm_node {
        let missing = vam_orchestrator::daemon::missing_vm_node_vars(
            |k| std::env::var(k).ok(),
            config.use_jailer,
        );
        if !missing.is_empty() {
            let msg = vam_orchestrator::daemon::vm_node_config_error(&missing);
            tracing::error!("{msg}");
            eprintln!("vamd: {msg}");
            std::process::exit(1);
        }
    }
    let jailed_node = vm_node && config.use_jailer;
    if jailed_node {
        let kernel = vam_orchestrator::golden::resolve_guest_kernel_path();
        let rootfs = vam_orchestrator::golden::resolve_rootfs_path();
        // The vsock sockets: the host's listener sockets are hard-linked into each jail, so
        // they live beside the jails (`/tmp` is often a tmpfs), in a root-only directory.
        let run_dir = jail_cfg.base_dir.join("run");
        let inputs = vam_core::PreflightInputs {
            firecracker_bin: &config.firecracker_bin,
            jailer_bin: &config.jailer_bin,
            shared_assets: &[
                ("guest kernel", kernel.as_path()),
                ("guest rootfs", rootfs.as_path()),
            ],
            same_fs: &[
                ("Drive9 root", drive9_root.as_path()),
                ("vsock run dir", run_dir.as_path()),
            ],
        };
        if let Err(e) = jail_cfg.preflight(&inputs) {
            tracing::error!("{e}");
            eprintln!("vamd: {e}");
            std::process::exit(1);
        }
        if let Err(e) = vam_core::jail::prepare_run_dir(&run_dir) {
            tracing::error!("cannot create the vsock run dir {}: {e}", run_dir.display());
            eprintln!(
                "vamd: cannot create the vsock run dir {}: {e}",
                run_dir.display()
            );
            std::process::exit(1);
        }
        vam_core::jail::set_run_dir(run_dir.clone());
        tracing::info!(
            uid = jail_cfg.uid,
            gid = jail_cfg.gid,
            cgroup_version = jail_cfg.cgroup_version,
            base_dir = %jail_cfg.base_dir.display(),
            "jailer on: Firecracker runs chrooted, as an unprivileged uid, in its own PID and mount namespaces"
        );
        tracing::info!(
            "golden snapshot bake and restore run inside the jailer; a session that cannot restore cold boots, still jailed"
        );
        vam_orchestrator::golden::set_jailed_node(jail_cfg.uid);
    } else if vm_node {
        tracing::warn!(
            "jailer OFF (--no-jailer or VAMD_USE_JAILER=0): Firecracker runs unconfined on this node, and the node reports `jailer off`"
        );
    }

    let report =
        run_startup_reconciliation(&store, &cid_allocator, None, Some(drive9_root.as_path()))
            .or_exit("startup reconciliation failed");

    tracing::info!("{}", report.to_json());
    // Terminal runs that never got a receipt (a crash between the end and the issue).
    let reconciled = vam_orchestrator::receipt::issue_missing_receipts(&store, 0);
    if reconciled > 0 {
        tracing::info!("[vamd] issued {reconciled} missing run receipt(s)");
    }
    if jailed_node {
        // Jails of Firecrackers that are gone (a crash, a kill -9): their directories
        // and `/tmp` links. Live ones stay for the reconciliation that adopted them.
        if let Some(exec) = config.firecracker_bin.file_name() {
            let tmp = std::env::temp_dir();
            let run = vam_core::jail::run_dir();
            let swept = vam_core::jail::sweep_stale_jails(
                &jail_cfg.base_dir,
                exec,
                &[tmp.as_path(), run.as_path()],
            );
            if !swept.is_empty() {
                tracing::info!(jails = ?swept, "removed stale jail directories");
            }
        }
    }
    #[cfg(feature = "process-guest")]
    if let Some(mode) = &process_mode {
        let n = vam_orchestrator::process_mode::sweep_known_sessions(&store, mode);
        tracing::info!(
            sessions = n,
            "process guests: swept leftovers of known sessions"
        );
    }

    tracing::info!("=== Phase 3: Pool Warmup ===");
    let net_pool = net_pool_for_runtime();
    tracing::info!("{}", net_pool.startup_message());
    tracing::info!("Warm pool initialized: 0 standby instances.");

    let fc_bin = config.firecracker_bin.clone();
    let jl_bin = config.jailer_bin.clone();
    let use_jl = config.use_jailer;
    let factory_jail_cfg = jail_cfg.clone();
    // API rate limits: on unless VAMD_RATE_LIMIT=off, which logs a WARN and is reported on the
    // node and each run's receipt.
    vam_orchestrator::api::rate_limit::install(vam_orchestrator::daemon::parse_rate_limits(|k| {
        std::env::var(k).ok()
    }));
    // The base this node boots: read once; a VM node hashes the rootfs through the golden
    // cache (the bake needs the same hash next).
    if let Err(e) =
        tokio::task::spawn_blocking(move || vam_orchestrator::node_base::install_from_env(vm_node))
            .await
    {
        tracing::warn!("base manifest load failed: {e}");
    }
    vam_orchestrator::isolation::install(vam_orchestrator::isolation::IsolationInputs {
        guest_mode: vam_orchestrator::guest_mode(),
        use_jailer: use_jl,
        // From the transport vamd resolved above, never a constant.
        api_tls: transport.is_tls(),
        rate_limits: vam_orchestrator::api::rate_limit::current(),
    });

    let vmm_factory: VmmFactory = Arc::new(move |vm_id, guest_cid| {
        Box::new(
            vam_core::FirecrackerVmm::new(
                vm_id.to_string(),
                fc_bin.clone(),
                jl_bin.clone(),
                use_jl,
                guest_cid,
            )
            .with_jail(factory_jail_cfg.clone()),
        )
    });

    #[cfg(feature = "process-guest")]
    let vmm_factory: VmmFactory = match &process_mode {
        Some(mode) => vam_orchestrator::process_mode::vmm_factory(mode),
        None => vmm_factory,
    };
    #[cfg(feature = "process-guest")]
    let drive9 = match &process_mode {
        Some(_) => {
            vam_orchestrator::manager::drive9_runtime_with(vam_storage::Drive9Backend::Directory)
        }
        None => drive9_runtime(),
    };
    #[cfg(not(feature = "process-guest"))]
    let drive9 = drive9_runtime();
    let (workspace_manager, catalog_cache) = drive9.or_exit("Drive9 stack init failed");

    // Sessions that died holding their workspace: commit what they did (or keep
    // it) before anything else can take the workspace. Process guests were
    // swept above, so nothing is still writing.
    let settled = vam_orchestrator::session_end::settle_dead_trees(
        &store,
        &workspace_manager,
        &report.dead_trees,
    )
    .await;
    if settled > 0 {
        tracing::info!(sessions = settled, "settled the trees of dead sessions");
    }

    vam_orchestrator::golden::warmup_golden_snapshot(
        vmm_factory.clone(),
        workspace_manager.clone(),
        net_pool.clone(),
        drive9_root.as_path(),
    )
    .await;

    tracing::info!("=== Phase 4: Ready ===");
    let (manager, cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());
    let manager_shutdown = manager.clone();
    let manager_sweeper = manager.clone();

    let manager_handle = tokio::spawn(run_manager_loop(
        store.clone(),
        vault.clone(),
        cid_allocator.clone(),
        vmm_factory.clone(),
        workspace_manager,
        catalog_cache,
        net_pool,
        cmd_rx,
        config.telemetry_capture,
    ));

    // The first admin key is created by `vamd --init`; a start with none mints nothing and
    // says what to do. The secret never reaches the log.
    vam_orchestrator::first_key::start_first_key(&store, config.bootstrap_api_key.clone())
        .or_exit("bootstrap API key setup failed");

    vam_orchestrator::connectors::install_config(
        config
            .connector_config()
            .or_exit("connector client id is invalid"),
    );

    // Trusted marketplaces (CN-4.4): where `git` runs from and how often each is read. The
    // preconfigured Squrell entry follows the node's marketplace domain when one is set.
    {
        let state_dir = config
            .db_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let mk = vam_orchestrator::marketplace::MarketplaceConfig::from_env(&state_dir);
        if mk.fetcher.git_bin.is_none() {
            tracing::warn!(
                "git was not found: marketplaces cannot be read until it is installed (or {} names it)",
                vam_orchestrator::marketplace::ENV_GIT_BIN
            );
        }
        vam_orchestrator::marketplace::install(mk);
        let squrell_domain = match registry_domains_log.marketplace_domain() {
            "" => vam_orchestrator::marketplace::SQURELL_DOMAIN,
            d => d,
        };
        if let Err(e) = store.sync_preconfigured_marketplace_domain(squrell_domain) {
            tracing::warn!("could not set the Squrell marketplace domain: {e}");
        }
    }

    let pdp = Arc::new(ControlPlanePdp::new().or_exit("control-plane Cedar PDP failed to compile"));

    let draining = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(true));

    let vault_for_delivery = vault.clone();
    let app_state = AppState {
        store: store.clone(),
        vault,
        manager,
        pdp,
        listen_addr: config.listen_addr.to_string(),
        draining: draining.clone(),
        ready: ready.clone(),
        e2e_mock_scripts: mock_scripts.is_some(),
    };

    let listener = tokio::net::TcpListener::bind(config.listen_addr)
        .await
        .or_exit("failed to bind the HTTP listener");
    tracing::info!(
        otlp_enabled = config.otlp_enabled,
        otlp_endpoint = %config.otlp_endpoint,
        "vamd HTTP listening on {} (after reconciliation)",
        config.listen_addr
    );
    transport.log(config.listen_addr);

    let server_ready = ready.clone();
    let server_handle = tokio::spawn(async move {
        if let Err(e) = api::serve_with(app_state, listener, transport).await {
            tracing::error!("HTTP server exited with error: {}", e);
            server_ready.store(false, Ordering::SeqCst);
        }
    });

    let telemetry_addr = config.telemetry_listen_addr;
    let telemetry_store = store.clone();
    let telemetry_bus = event_bus.clone();
    let telemetry_handle = tokio::spawn(async move {
        tracing::info!(
            "vamd telemetry gRPC listening on {} (StreamEvents; grpcurl -plaintext {} vam.v1.TelemetryStreamService/StreamEvents)",
            telemetry_addr,
            telemetry_addr
        );
        if let Err(e) = serve_telemetry_grpc(telemetry_addr, telemetry_store, telemetry_bus).await {
            tracing::error!("telemetry gRPC server exited with error: {}", e);
        }
    });

    // Each enabled marketplace is read on its own timer (default every 6 hours) and on demand.
    // A process-guest test node drives every read itself.
    if vam_orchestrator::marketplace::periodic_refresh_enabled() {
        let store_marketplaces = store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                vam_orchestrator::marketplace::refresh_due(store_marketplaces.clone()).await;
            }
        });
    }

    let store_sweeper = store.clone();
    let sweeper_layout = drive9_layout();
    let idle_ttl_secs = config.session_idle_ttl_secs;
    if let Some(warning) = &config.session_idle_ttl_warning {
        tracing::warn!("{warning}");
    }
    if let Some(secs) = idle_ttl_secs {
        tracing::info!("idle sessions end after {secs}s without activity");
    }
    let sweeper_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            match vam_orchestrator::api::sweep_gate_timeouts(&manager_sweeper, &store_sweeper).await
            {
                Ok(0) => {}
                Ok(resumed) => {
                    tracing::info!("[vamd] resumed {} gate timeout(s)", resumed);
                }
                Err(e) => tracing::error!("[vamd] gate timeout sweeper error: {}", e),
            }
            match sweep_expired_approvals(&store_sweeper, &manager_sweeper, &sweeper_layout).await {
                Ok(0) => {}
                Ok(expired) => {
                    tracing::info!("[vamd] expired {} stale HITL approval(s)", expired);
                }
                Err(e) => tracing::error!("[vamd] HITL expiry sweeper error: {}", e),
            }
            match store_sweeper.prune_notifications(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
            ) {
                Ok(0) => {}
                Ok(pruned) => tracing::info!("[vamd] pruned {} notification(s)", pruned),
                Err(e) => tracing::error!("[vamd] notification prune error: {}", e),
            }
            match sweep_active_duration(&store_sweeper, &manager_sweeper, &sweeper_layout).await {
                Ok(0) => {}
                Ok(expired) => {
                    tracing::info!("[vamd] expired {} run(s) past active max_duration", expired);
                }
                Err(e) => tracing::error!("[vamd] active duration sweeper error: {}", e),
            }
            if let Some(ttl_secs) = idle_ttl_secs {
                match vam_orchestrator::ttl::sweep_idle_sessions(
                    &store_sweeper,
                    &manager_sweeper,
                    &sweeper_layout,
                    ttl_secs,
                )
                .await
                {
                    Ok(0) => {}
                    Ok(ended) => {
                        tracing::info!("[vamd] ended {} idle session(s)", ended);
                    }
                    Err(e) => tracing::error!("[vamd] idle session sweeper error: {}", e),
                }
            }
            {
                let store = store_sweeper.clone();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                match tokio::task::spawn_blocking(move || store.sweep_expiring_grants(now)).await {
                    Ok(Ok(0)) => {}
                    Ok(Ok(sent)) => {
                        tracing::info!("[vamd] sent {} grant.expiring event(s)", sent);
                    }
                    Ok(Err(e)) => tracing::error!("[vamd] grant expiry sweeper error: {}", e),
                    Err(e) => tracing::error!("[vamd] grant expiry sweeper join error: {}", e),
                }
            }
            match sweep_keep_warm(&store_sweeper, &manager_sweeper, &sweeper_layout).await {
                Ok(0) => {}
                Ok(parked) => {
                    tracing::info!("[vamd] parked {} run(s) after keep-warm", parked);
                }
                Err(e) => tracing::error!("[vamd] keep-warm sweeper error: {}", e),
            }
        }
    });

    // Catch-all for terminal paths that issue no receipt themselves. The grace
    // keeps it from racing a finalizer that is still making the run's receipt.
    let receipt_store = store.clone();
    let receipt_sweeper_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            let store = receipt_store.clone();
            let _ = tokio::task::spawn_blocking(move || {
                vam_orchestrator::receipt::issue_missing_receipts(&store, 10)
            })
            .await;
        }
    });

    // Channel deliveries (NT-2.3): due rows go to the channel broker, the bot token read from the
    // vault for each call. The loop keeps no credential.
    let delivery_store = store.clone();
    let delivery_vault = vault_for_delivery;
    let delivery_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            vam_orchestrator::notify_delivery::run_once(&delivery_store, &delivery_vault).await;
        }
    });

    let retention_dir = tenant_spool_dir.clone();
    let retention_mgr = retention.clone();
    let retention_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let report = vam_telemetry::sweep_tenant_spool(&retention_dir, &retention_mgr);
            if report.freed_bytes > 0 {
                tracing::info!(
                    freed_bytes = report.freed_bytes,
                    files_pruned = report.files_pruned,
                    class_b = report.class_b_bytes,
                    class_c = report.class_c_bytes,
                    "retention sweeper pruned Class C then B"
                );
            }
            if report.class_a_over_quota {
                tracing::error!(
                    class_a_bytes = report.class_a_bytes,
                    "Class A audit over quota; new audit appends fail-closed"
                );
            }
        }
    });

    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm =
            signal(SignalKind::terminate()).or_exit("cannot register the SIGTERM handler");
        let mut sigint =
            signal(SignalKind::interrupt()).or_exit("cannot register the SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received. Shutting down...");
            }
            _ = sigint.recv() => {
                tracing::info!("SIGINT received. Shutting down (Ctrl+C again to force exit)...");
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("Ctrl+C received. Shutting down...");
    }

    draining.store(true, Ordering::SeqCst);
    ready.store(false, Ordering::SeqCst);
    server_handle.abort();
    telemetry_handle.abort();
    sweeper_handle.abort();
    receipt_sweeper_handle.abort();
    delivery_handle.abort();
    retention_handle.abort();
    egress_handle.abort();
    drop(manager_shutdown);
    manager_handle.abort();

    // Don't wait forever on session create / broker tasks. Force-exit so cargo run
    // and interactive terminals return promptly.
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(2)) => {
            tracing::warn!("Shutdown grace period elapsed; forcing process exit.");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::warn!("Second interrupt; forcing immediate process exit.");
        }
    }

    tracing::info!("vamd has terminated.");
    vam_telemetry::shutdown_telemetry();
    // Hard exit: background session/broker tasks and Firecracker children must not
    // keep the Tokio runtime (and `cargo run`) alive after Ctrl+C.
    std::process::exit(0);
}
