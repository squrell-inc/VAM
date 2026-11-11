use once_cell::sync::Lazy;
use sha2::{Digest as ShaDigest, Sha256};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{sleep, timeout};

use crate::primitives::{self, is_builtin};
use crate::RuntimeError;
use vam_spec::v1::ErrorStatus;

/// Command lookup path injected into every guest tool subprocess.
///
/// `/usr/local/bin` is where `scripts/build-rootfs.sh` installs `bun`.
/// `guest_path_covers_rootfs_executable_dirs` fails if that script starts
/// installing executables into a directory this string does not contain.
#[cfg(unix)]
const GUEST_TOOL_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Track currently active process groups per lease ID to allow cancellation.
static ACTIVE_PROCESS_GROUPS: Lazy<Mutex<HashMap<String, u32>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Register an active lease ID with its spawned process group ID (pgid).
pub fn register_active_lease(lease_id: String, pgid: u32) {
    if let Ok(mut lock) = ACTIVE_PROCESS_GROUPS.lock() {
        lock.insert(lease_id, pgid);
    }
}

/// Unregister an active lease ID when it finishes.
pub fn unregister_active_lease(lease_id: &str) {
    if let Ok(mut lock) = ACTIVE_PROCESS_GROUPS.lock() {
        lock.remove(lease_id);
    }
}

/// Cancel a task by sending SIGKILL to its process group.
pub fn cancel_active_lease(lease_id: &str) -> bool {
    if let Ok(mut lock) = ACTIVE_PROCESS_GROUPS.lock() {
        if let Some(&_pgid) = lock.get(lease_id) {
            // Send SIGKILL to the whole process group (-pgid)
            #[cfg(target_family = "unix")]
            kill_process_group(_pgid, libc::SIGKILL);
            lock.remove(lease_id);
            return true;
        }
    }
    false
}

/// Signals a tool's process group. Refuses pgid 0 and 1: `kill(-1, sig)` would
/// signal every process the caller may signal, and `kill(0, sig)` its own group.
#[cfg(target_family = "unix")]
fn kill_process_group(pgid: u32, sig: i32) {
    if pgid <= 1 || pgid > i32::MAX as u32 {
        return;
    }
    unsafe {
        libc::kill(-(pgid as i32), sig);
    }
}

/// Kill every registered tool process group (the guest is shutting down).
/// Never signals anything that was not registered by a lease.
pub fn cancel_all_active_leases() -> usize {
    let mut n = 0;
    if let Ok(mut lock) = ACTIVE_PROCESS_GROUPS.lock() {
        for &_pgid in lock.values() {
            #[cfg(target_family = "unix")]
            kill_process_group(_pgid, libc::SIGKILL);
            n += 1;
        }
        lock.clear();
    }
    n
}

/// Dynamic registry of allowed guest tools.
pub struct ToolRegistry;

impl ToolRegistry {
    pub fn resolve_dynamic(tool_name: &str) -> Option<(String, Vec<String>)> {
        Self::resolve_dynamic_at(tool_name, &crate::primitives::skills_mount_root())
    }

    /// Resolve `tool_name` by scanning `skills_dir` for `manifest.json` files.
    ///
    /// `localExec.entrypoint` is relative to the directory that contains that
    /// manifest. The returned argv path is the joined file, so a skill still
    /// runs after its install directory is renamed.
    pub fn resolve_dynamic_at(
        tool_name: &str,
        skills_dir: &std::path::Path,
    ) -> Option<(String, Vec<String>)> {
        // Try static first
        #[cfg(windows)]
        match tool_name {
            "echo" => {
                return Some((
                    "cmd.exe".to_string(),
                    vec!["/C".to_string(), "echo".to_string()],
                ))
            }
            "sh" => return Some(("cmd.exe".to_string(), vec!["/C".to_string()])),
            _ => {}
        }
        #[cfg(not(windows))]
        match tool_name {
            "echo" => return Some(("/bin/echo".to_string(), vec![])),
            "sh" => return Some(("/bin/sh".to_string(), vec!["-c".to_string()])),
            "cat" => return Some(("/bin/cat".to_string(), vec![])),
            "id" => return Some(("/bin/id".to_string(), vec![])),
            "env" => return Some(("/usr/bin/env".to_string(), vec![])),
            _ => {}
        }

        if !skills_dir.is_dir() {
            return None;
        }

        if let Ok(entries) = std::fs::read_dir(skills_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let manifest_path = path.join("manifest.json");
                    if manifest_path.is_file() {
                        if let Ok(content) = std::fs::read_to_string(&manifest_path) {
                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                                if let Some(tools) = val.get("tools").and_then(|v| v.as_array()) {
                                    for tool in tools {
                                        if tool.get("name").and_then(|v| v.as_str())
                                            == Some(tool_name)
                                        {
                                            if let Some(local_exec) = tool
                                                .get("localExec")
                                                .or_else(|| tool.get("local_exec"))
                                            {
                                                let command = local_exec
                                                    .get("command")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("");
                                                let entrypoint = local_exec
                                                    .get("entrypoint")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("");

                                                // Validate command (allowlisted interpreters only)
                                                let resolved_cmd = match command {
                                                    "bun" => "/usr/local/bin/bun".to_string(),
                                                    "node" => "/usr/local/bin/node".to_string(),
                                                    "python" | "python3" => {
                                                        "/usr/bin/python3".to_string()
                                                    }
                                                    other if other.starts_with('/') => {
                                                        other.to_string()
                                                    }
                                                    _ => continue, // forbidden interpreter
                                                };

                                                let resolved_entrypoint = match vam_spec::resolve_manifest_relative_entrypoint(
                                                    &path,
                                                    entrypoint,
                                                ) {
                                                    Ok(resolved) => resolved,
                                                    Err(_) => continue,
                                                };
                                                let Some(resolved_entrypoint) =
                                                    resolved_entrypoint.to_str()
                                                else {
                                                    continue;
                                                };

                                                let mut args =
                                                    vec![resolved_entrypoint.to_string()];
                                                if let Some(default_args) = local_exec
                                                    .get("defaultArgs")
                                                    .or_else(|| local_exec.get("default_args"))
                                                    .and_then(|v| v.as_array())
                                                {
                                                    for arg in default_args {
                                                        if let Some(arg_str) = arg.as_str() {
                                                            args.push(arg_str.to_string());
                                                        }
                                                    }
                                                }
                                                return Some((resolved_cmd, args));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }
}

/// Computes the content digest for the target script or command being executed.
/// For inline shell (`sh`), hashes the command string bytes.
/// For skills and dynamic scripts, opens the entrypoint file and computes its SHA-256 hash.
pub fn compute_script_digest(
    tool_name: &str,
    args_struct: Option<&prost_types::Struct>,
) -> (String, Option<String>) {
    if is_builtin(tool_name) {
        return (primitives::builtin_digest(tool_name), None);
    }

    if tool_name == "sh" {
        if let Some(s) = args_struct {
            let cmd_str = if let Some(prost_types::value::Kind::ListValue(list)) =
                s.fields.get("args").and_then(|v| v.kind.as_ref())
            {
                list.values
                    .iter()
                    .filter_map(|v| {
                        if let Some(prost_types::value::Kind::StringValue(ref sv)) = v.kind {
                            Some(sv.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            } else if let Some(prost_types::value::Kind::StringValue(ref sv)) =
                s.fields.get("command").and_then(|v| v.kind.as_ref())
            {
                sv.clone()
            } else {
                String::new()
            };
            let digest = format!("{:x}", Sha256::digest(cmd_str.as_bytes()));
            return (digest, None);
        }
        return (format!("{:x}", Sha256::digest(b"")), None);
    }

    if matches!(tool_name, "echo" | "cat" | "id" | "env") {
        return (String::new(), None);
    }

    if let Some((_cmd, template_args)) = ToolRegistry::resolve_dynamic(tool_name) {
        if let Some(entrypoint) = template_args.first() {
            if let Ok(mut file) = std::fs::File::open(entrypoint) {
                use std::io::Read;
                let mut bytes = Vec::new();
                if file.read_to_end(&mut bytes).is_ok() {
                    let digest = format!("{:x}", Sha256::digest(&bytes));
                    return (digest, Some(entrypoint.clone()));
                }
            }
        }
    }

    if let Some(s) = args_struct {
        for key in ["file", "path", "script", "filepath"] {
            if let Some(prost_types::value::Kind::StringValue(ref path_str)) =
                s.fields.get(key).and_then(|v| v.kind.as_ref())
            {
                if let Ok(mut file) = std::fs::File::open(path_str) {
                    use std::io::Read;
                    let mut bytes = Vec::new();
                    if file.read_to_end(&mut bytes).is_ok() {
                        let digest = format!("{:x}", Sha256::digest(&bytes));
                        return (digest, Some(path_str.clone()));
                    }
                }
            }
        }
    }

    (String::new(), None)
}

/// Reads `/proc/<pid>/stat` and `/proc/<pid>/status` to compute CPU time (ms) and peak RSS (bytes).
pub fn sample_proc_metrics(pid: u32) -> (u64, u64) {
    let mut cpu_time_ms = 0u64;
    let mut peak_rss_bytes = 0u64;

    // 1. /proc/<pid>/stat for utime + stime
    if let Ok(stat_content) = std::fs::read_to_string(format!("/proc/{}/stat", pid)) {
        if let Some(idx) = stat_content.rfind(')') {
            let rest = &stat_content[idx + 1..];
            let fields: Vec<&str> = rest.split_whitespace().collect();
            // fields[11] is utime (14th field in full stat), fields[12] is stime (15th field)
            if fields.len() > 12 {
                let utime: u64 = fields[11].parse().unwrap_or(0);
                let stime: u64 = fields[12].parse().unwrap_or(0);
                // System clock ticks per second (usually 100 on Linux)
                let ticks_per_sec = 100u64;
                cpu_time_ms = ((utime + stime) * 1000) / ticks_per_sec;
            }
        }
    }

    // 2. /proc/<pid>/status for VmHWM / VmRSS
    if let Ok(status_content) = std::fs::read_to_string(format!("/proc/{}/status", pid)) {
        for line in status_content.lines() {
            if line.starts_with("VmHWM:") || (peak_rss_bytes == 0 && line.starts_with("VmRSS:")) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(kb) = parts[1].parse::<u64>() {
                        peak_rss_bytes = kb * 1024;
                        if line.starts_with("VmHWM:") {
                            break;
                        }
                    }
                }
            }
        }
    }

    (cpu_time_ms, peak_rss_bytes)
}

#[derive(Debug, Clone, Default)]
pub struct SubprocessToolResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub system_logs: String,
    pub exit_code: i32,
    pub error: Option<ErrorStatus>,
    pub resource_usage: Option<vam_spec::v1::ResourceUsage>,
}

/// Runs a subprocess unprivileged with strict env scrubbing, setuid/setgid privilege drops,
/// a hard timeout, and bounded concurrent output capture.
pub async fn execute_tool_subprocess(
    tool_name: &str,
    args_struct: Option<&prost_types::Struct>,
    timeout_duration: Duration,
    lease_id: Option<String>,
) -> Result<SubprocessToolResult, RuntimeError> {
    execute_tool_subprocess_with_token(
        tool_name,
        args_struct,
        timeout_duration,
        lease_id,
        None,
        None,
        None,
    )
    .await
}

pub async fn execute_tool_subprocess_with_trace(
    tool_name: &str,
    args_struct: Option<&prost_types::Struct>,
    timeout_duration: Duration,
    lease_id: Option<String>,
    trace_context: Option<&vam_spec::v1::TraceContext>,
) -> Result<SubprocessToolResult, RuntimeError> {
    execute_tool_subprocess_with_token(
        tool_name,
        args_struct,
        timeout_duration,
        lease_id,
        None,
        None,
        trace_context,
    )
    .await
}

pub async fn execute_tool_subprocess_with_token(
    tool_name: &str,
    args_struct: Option<&prost_types::Struct>,
    timeout_duration: Duration,
    lease_id: Option<String>,
    capability_token: Option<&vam_spec::v1::SignedEnvelope>,
    verifying_key: Option<&ed25519_dalek::VerifyingKey>,
    trace_context: Option<&vam_spec::v1::TraceContext>,
) -> Result<SubprocessToolResult, RuntimeError> {
    let (script_digest, _script_path) = compute_script_digest(tool_name, args_struct);

    // If a capability token is provided, cryptographically verify it binds this tool, args digest, and script digest
    let claims_opt = if let Some(token) = capability_token {
        let args_digest = vam_spec::canonical_args_digest(args_struct)
            .map_err(|e| RuntimeError::Handler(format!("arguments digest: {e}")))?;
        Some(crate::pep::verify_guest_capability_token(
            token,
            verifying_key,
            tool_name,
            &args_digest,
            &script_digest,
            None,
            lease_id.as_deref(),
        )?)
    } else {
        None
    };

    // Builtins (except `run`) execute in-process after token verification.
    if is_builtin(tool_name) && tool_name != "run" {
        let start_builtin = std::time::Instant::now();
        match primitives::execute_builtin(tool_name, args_struct) {
            Ok(result) => {
                let mut extra = HashMap::new();
                extra.insert("tool.builtin".to_string(), "true".to_string());
                record_tool_exec_telemetry(
                    tool_name,
                    lease_id.as_deref(),
                    0,
                    start_builtin.elapsed(),
                    trace_context,
                    &result.stdout,
                    &result.stderr,
                    extra,
                );
                return Ok(result);
            }
            Err(msg) => {
                let marked = format!("tool_error: {}", msg);
                let stderr = marked.as_bytes().to_vec();
                let mut extra = HashMap::new();
                extra.insert("tool.builtin".to_string(), "true".to_string());
                extra.insert("tool.error".to_string(), "true".to_string());
                record_tool_exec_telemetry(
                    tool_name,
                    lease_id.as_deref(),
                    1,
                    start_builtin.elapsed(),
                    trace_context,
                    &[],
                    &stderr,
                    extra,
                );
                return Ok(SubprocessToolResult {
                    stdout: Vec::new(),
                    stderr,
                    system_logs: String::new(),
                    exit_code: 1,
                    error: Some(ErrorStatus {
                        code: 3, // INVALID_ARGUMENT
                        message: marked,
                        details: vec![],
                        retryable: false,
                    }),
                    resource_usage: None,
                });
            }
        }
    }

    // `run` builtin delegates to the existing unprivileged sh supervisor.
    let (effective_tool_name, effective_args) = if tool_name == "run" {
        let mut sh_args = prost_types::Struct::default();
        if let Some(s) = args_struct {
            if let Some(prost_types::value::Kind::StringValue(cmd)) =
                s.fields.get("command").and_then(|v| v.kind.as_ref())
            {
                sh_args.fields.insert(
                    "args".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::ListValue(
                            prost_types::ListValue {
                                values: vec![prost_types::Value {
                                    kind: Some(prost_types::value::Kind::StringValue(cmd.clone())),
                                }],
                            },
                        )),
                    },
                );
            }
        }
        ("sh", Some(sh_args))
    } else {
        (tool_name, args_struct.cloned())
    };

    let effective_args_ref = effective_args.as_ref();

    // Resolve resource envelope from capability claims or fallback to default envelope
    let default_envelope = vam_spec::v1::ResourceEnvelope {
        max_wall_time_ms: timeout_duration.as_millis() as u64,
        max_cpu_time_ms: 30_000,
        max_memory_bytes: 1024 * 1024 * 1024, // 1 GiB
        max_procs: 64,
        max_open_files: 256,
        max_file_size_bytes: 100 * 1024 * 1024, // 100 MiB
        max_egress_bytes: 50 * 1024 * 1024,
        max_output_bytes: 2 * 1024 * 1024,
    };

    let envelope = claims_opt
        .as_ref()
        .and_then(|c| c.resource_envelope.clone())
        .unwrap_or(default_envelope);

    let max_wall_ms = envelope.max_wall_time_ms.max(100);
    let effective_timeout = Duration::from_millis(max_wall_ms);

    #[allow(unused_variables)]
    let max_cpu_sec = (envelope.max_cpu_time_ms / 1000).max(1);
    #[allow(unused_variables)]
    let max_cpu_hard = max_cpu_sec + 2;
    #[allow(unused_variables)]
    let max_mem = envelope.max_memory_bytes.max(64 * 1024 * 1024);
    #[allow(unused_variables)]
    let max_procs = envelope.max_procs.max(8) as u64;
    #[allow(unused_variables)]
    let max_files = envelope.max_open_files.max(32) as u64;
    #[allow(unused_variables)]
    let max_fsize = envelope.max_file_size_bytes.max(1024 * 1024);
    let max_cap = (envelope.max_output_bytes as usize).max(64 * 1024);

    // 1. Resolve tool dynamically
    let (exec_path, mut template_args) = ToolRegistry::resolve_dynamic(effective_tool_name)
        .ok_or_else(|| {
            RuntimeError::Handler(format!("Unimplemented or forbidden tool: {}", tool_name))
        })?;

    // 2. Parse arguments from prost Struct if any
    if let Some(s) = effective_args_ref {
        // Unpack "args" array or use the struct key/values
        if let Some(prost_types::value::Kind::ListValue(list)) =
            s.fields.get("args").and_then(|v| v.kind.as_ref())
        {
            for val in &list.values {
                if let Some(prost_types::value::Kind::StringValue(ref s_val)) = val.kind {
                    template_args.push(s_val.clone());
                } else if let Some(prost_types::value::Kind::NumberValue(n_val)) = val.kind {
                    template_args.push(n_val.to_string());
                } else if let Some(prost_types::value::Kind::BoolValue(b_val)) = val.kind {
                    template_args.push(b_val.to_string());
                }
            }
        }
    }

    // 3. Build Command with complete Env Scrubbing
    let is_jit = exec_path.contains("bun") || exec_path.contains("node");
    let mut cmd = Command::new(&exec_path);
    cmd.args(&template_args);
    cmd.env_clear();

    // Inject strict minimal environment
    #[cfg(unix)]
    cmd.env("PATH", GUEST_TOOL_PATH);
    #[cfg(windows)]
    {
        let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        cmd.env("SystemRoot", &sysroot);
        cmd.env("PATH", format!("{}\\System32", sysroot));
    }
    cmd.env("HOME", "/home/guest");
    cmd.env("LANG", "en_US.UTF-8");
    cmd.env("TMPDIR", "/tmp");

    // Bun/Node fetch ignores NO_PROXY and deadlocks when HTTP_PROXY points at the
    // in-guest loopback proxy (same origin as the explicit 127.0.0.1:3128 rewrite).
    // JS skills must call the proxy explicitly (X-Target-Url), not via env proxy.
    // Set empty strings so a broken env_clear cannot inherit a parent HTTP_PROXY.
    if is_jit {
        cmd.env("HTTP_PROXY", "");
        cmd.env("HTTPS_PROXY", "");
        cmd.env("http_proxy", "");
        cmd.env("https_proxy", "");
        cmd.env("ALL_PROXY", "");
        cmd.env("all_proxy", "");
        cmd.env("NO_PROXY", "*");
        cmd.env("no_proxy", "*");
    } else {
        let http_proxy = std::env::var("HTTP_PROXY").unwrap_or_else(|_| crate::egress::proxy_url());
        let https_proxy =
            std::env::var("HTTPS_PROXY").unwrap_or_else(|_| crate::egress::proxy_url());
        let no_proxy =
            std::env::var("NO_PROXY").unwrap_or_else(|_| "127.0.0.1,localhost".to_string());
        cmd.env("HTTP_PROXY", &http_proxy);
        cmd.env("HTTPS_PROXY", &https_proxy);
        cmd.env("NO_PROXY", &no_proxy);
        cmd.env("http_proxy", &http_proxy);
        cmd.env("https_proxy", &https_proxy);
        cmd.env("no_proxy", &no_proxy);
    }

    // Where this guest's egress proxy listens. bun and node get no HTTP_PROXY (above), so a JS
    // tool reads this to reach the proxy; a process guest's port is not fixed.
    cmd.env("VAM_EGRESS_ENDPOINT", crate::egress::proxy_url());

    // Inject per-session placeholder and integration env vars
    for (k, v) in std::env::vars() {
        if k.starts_with("VAM_SECRET_") || k.starts_with("vamsec_") || k.contains("PLACEHOLDER") {
            cmd.env(k, v);
        }
    }

    // Phase 4.4: Inject OTEL_TRACEPARENT and OTEL_TRACESTATE into tool subprocess env
    if let Some(tc) = trace_context {
        if !tc.traceparent.is_empty() {
            cmd.env("OTEL_TRACEPARENT", &tc.traceparent);
        }
        if !tc.tracestate.is_empty() {
            cmd.env("OTEL_TRACESTATE", &tc.tracestate);
        }
    }

    // Confinement to the workspace root (/mnt/drive9/workspace unless
    // VAM_WORKSPACE_ROOT is set), or /work, or /tmp if running host-side tests
    let ws_root = crate::primitives::workspace_root();
    if ws_root.is_dir() {
        cmd.current_dir(&ws_root);
    } else if std::path::Path::new("/work").is_dir() {
        cmd.current_dir("/work");
    } else {
        cmd.current_dir("/tmp");
    }

    // Configure standard streams redirection
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let stdin_bytes = if let Some(s) = effective_args_ref {
        let json_val = struct_to_json(s);
        Some(json_val.to_string())
    } else {
        None
    };

    if stdin_bytes.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }

    // 4. Setuid, setgid, setsid, rlimit, and Landlock in pre_exec
    // The Landlock rule paths are resolved here, before fork: the closure below
    // must not allocate or read env.
    #[cfg(target_os = "linux")]
    let landlock_policy = crate::landlock::LandlockPolicy::from_configured_roots();
    #[cfg(target_os = "linux")]
    unsafe {
        cmd.pre_exec(move || {
            // Create a new session and process group
            libc::setsid();

            // Physics-based Resource Limits (S2) - Fail-Closed
            // 1. CPU time limit
            let cpu_limit = libc::rlimit {
                rlim_cur: max_cpu_sec,
                rlim_max: max_cpu_hard,
            };
            if libc::setrlimit(libc::RLIMIT_CPU, &cpu_limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }

            // 2. Virtual memory cap (RLIMIT_AS)
            // Skip for JIT-based runtimes like bun/node which allocate large virtual address spaces on startup
            if !is_jit {
                let mem_limit = libc::rlimit {
                    rlim_cur: max_mem,
                    rlim_max: max_mem,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &mem_limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            // 3. Max processes / threads. Not in a `process-guest` build: there the
            // guest is a host process, and Linux counts every thread of the host UID
            // against RLIMIT_NPROC, so a busy host made `sleep` fail with "Cannot fork".
            if let Some(proc_limit) = nproc_limit(max_procs) {
                if libc::setrlimit(libc::RLIMIT_NPROC, &proc_limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            // 4. Max open file descriptors
            let nofile_limit = libc::rlimit {
                rlim_cur: max_files,
                rlim_max: max_files,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &nofile_limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }

            // 5. Max file size write cap
            let fsize_limit = libc::rlimit {
                rlim_cur: max_fsize,
                rlim_max: max_fsize,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &fsize_limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }

            // 6. Disable core dumps (prevent disk leakage)
            let core_limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &core_limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }

            // In-guest Landlock LSM sandboxing (Proposal B / S4) - Fail-Closed
            match crate::landlock::apply_landlock_sandbox(&landlock_policy) {
                Ok(_) => {} // Ok(true) = applied, Ok(false) = kernel unsupported (graceful fallback)
                Err(err) => return Err(err), // Real initialization failure -> abort execution!
            }

            // Drop privileges if running as root (inside the VM)
            if libc::getuid() == 0 {
                libc::setgroups(0, std::ptr::null());
                if libc::setgid(100) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setuid(1000) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }

    let start_instant = std::time::Instant::now();

    // Spawn!
    let mut child = cmd
        .spawn()
        .map_err(|e| RuntimeError::Handler(format!("Failed to spawn child: {}", e)))?;
    let child_id = child.id();
    let pid = child_id;

    if let Some(ref l_id) = lease_id {
        if let Some(p) = child_id {
            register_active_lease(l_id.clone(), p);
        }
    }

    if let Some(ref stdin_str) = stdin_bytes {
        if let Some(mut child_stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = child_stdin.write_all(stdin_str.as_bytes()).await;
            let _ = child_stdin.flush().await;
            drop(child_stdin); // Send EOF
        }
    }

    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| RuntimeError::Handler("Stdout pipe missing".to_string()))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| RuntimeError::Handler("Stderr pipe missing".to_string()))?;

    // Read streams concurrently with bounded buffers
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();

    let mut stdout_truncated = false;
    let mut stderr_truncated = false;
    let mut stdout_eof = false;
    let mut stderr_eof = false;

    let mut stdout_buf = [0u8; 8192];
    let mut stderr_buf = [0u8; 8192];

    let io_future = async {
        loop {
            if (stdout_truncated || stdout_eof) && (stderr_truncated || stderr_eof) {
                break;
            }
            tokio::select! {
                res = stdout_pipe.read(&mut stdout_buf), if !stdout_truncated && !stdout_eof => {
                    match res {
                        Ok(0) => {
                            stdout_eof = true;
                        }
                        Ok(n) => {
                            if stdout_bytes.len() + n > max_cap {
                                let rem = max_cap.saturating_sub(stdout_bytes.len());
                                stdout_bytes.extend_from_slice(&stdout_buf[..rem]);
                                stdout_bytes.extend_from_slice(b"\n[Stdout truncated at limit]");
                                stdout_truncated = true;
                            } else {
                                stdout_bytes.extend_from_slice(&stdout_buf[..n]);
                            }
                        }
                        Err(_) => {
                            stdout_eof = true;
                        }
                    }
                }
                res = stderr_pipe.read(&mut stderr_buf), if !stderr_truncated && !stderr_eof => {
                    match res {
                        Ok(0) => {
                            stderr_eof = true;
                        }
                        Ok(n) => {
                            if stderr_bytes.len() + n > max_cap {
                                let rem = max_cap.saturating_sub(stderr_bytes.len());
                                stderr_bytes.extend_from_slice(&stderr_buf[..rem]);
                                stderr_bytes.extend_from_slice(b"\n[Stderr truncated at limit]");
                                stderr_truncated = true;
                            } else {
                                stderr_bytes.extend_from_slice(&stderr_buf[..n]);
                            }
                        }
                        Err(_) => {
                            stderr_eof = true;
                        }
                    }
                }
            }
        }
    };

    // 5. Hard timeout with escalating kills
    let status_res = timeout(effective_timeout, async {
        let (res, _) = tokio::join!(child.wait(), io_future);
        res
    })
    .await;

    if let Some(ref l_id) = lease_id {
        unregister_active_lease(l_id);
    }

    let (sampled_cpu, sampled_rss) = if let Some(p) = pid {
        sample_proc_metrics(p)
    } else {
        (0, 0)
    };
    let wall_time_ms = start_instant.elapsed().as_millis() as u64;

    match status_res {
        Ok(Ok(status)) => {
            let system_logs = if stdout_truncated || stderr_truncated {
                "[vam-runtime] Subprocess output exceeded limits and was truncated.\n".to_string()
            } else {
                String::new()
            };

            let exit_code = status.code().unwrap_or(0);
            #[cfg(target_family = "unix")]
            let exit_signal = {
                use std::os::unix::process::ExitStatusExt;
                status.signal()
            };
            #[cfg(not(target_family = "unix"))]
            let exit_signal: Option<i32> = None;

            let mut limit_exceeded = false;
            let mut limit_type = String::new();

            // S3: Legible Denial Translation for Agent Reasoning
            let error = if status.success() {
                None
            } else {
                let err_msg = String::from_utf8_lossy(&stderr_bytes);

                // Structured Signal / Exit Code Classification first (Fix F2, F5)
                let (code, legible_msg) = if exit_signal == Some(24)
                    || err_msg.contains("CPU time limit exceeded")
                {
                    limit_exceeded = true;
                    limit_type = "RLIMIT_CPU".to_string();
                    (
                        8, // RESOURCE_EXHAUSTED
                        "Resource Limit Exceeded (SIGXCPU): The process exceeded its allocated CPU time budget (RLIMIT_CPU).".to_string(),
                    )
                } else if exit_signal == Some(25)
                    || err_msg.contains("File too large")
                    || err_msg.contains("EFBIG")
                {
                    limit_exceeded = true;
                    limit_type = "RLIMIT_FSIZE".to_string();
                    (
                        8, // RESOURCE_EXHAUSTED
                        "Resource Limit Exceeded (EFBIG/SIGXFSZ): The process exceeded the maximum file size write cap (RLIMIT_FSIZE).".to_string(),
                    )
                } else if exit_signal == Some(9) {
                    limit_exceeded = true;
                    limit_type = "RLIMIT_AS/RLIMIT_NPROC".to_string();
                    (
                        8, // RESOURCE_EXHAUSTED
                        "Resource Limit Exceeded (SIGKILL): The process was terminated by signal 9 (likely out of memory / RLIMIT_AS or max process count / RLIMIT_NPROC).".to_string(),
                    )
                } else if exit_signal == Some(15) {
                    limit_type = "SIGTERM".to_string();
                    (
                        1, // CANCELLED
                        "Process Terminated (SIGTERM): The process was cancelled or terminated."
                            .to_string(),
                    )
                } else if exit_code == 124 {
                    limit_exceeded = true;
                    limit_type = "WALL_CLOCK_TIMEOUT".to_string();
                    (
                        8, // RESOURCE_EXHAUSTED
                        "Resource Limit Exceeded (Timeout): The process exceeded the wall-clock execution timeout.".to_string(),
                    )
                } else if err_msg.contains("Permission denied")
                    || err_msg.contains("EACCES")
                    || err_msg.contains("EPERM")
                    || err_msg.contains("Operation not permitted")
                {
                    (
                        7, // PERMISSION_DENIED
                        format!(
                            "Sandbox Policy Denied (EACCES/EPERM): The tool attempted an unauthorized write or operation outside allowed workspace paths. Detail: {}",
                            err_msg.trim()
                        ),
                    )
                } else if err_msg.contains("MemoryError")
                    || err_msg.contains("Out of memory")
                    || err_msg.contains("Cannot allocate memory")
                {
                    limit_exceeded = true;
                    limit_type = "RLIMIT_AS".to_string();
                    (
                        8, // RESOURCE_EXHAUSTED
                        "Resource Limit Exceeded (MemoryError): The process exceeded memory limits (RLIMIT_AS).".to_string(),
                    )
                } else {
                    let display_code = if let Some(sig) = exit_signal {
                        format!("signal {}", sig)
                    } else {
                        format!("code {}", exit_code)
                    };
                    let stdout_msg = String::from_utf8_lossy(&stdout_bytes);
                    let combined_msg = if !err_msg.trim().is_empty() {
                        err_msg.trim().to_string()
                    } else if !stdout_msg.trim().is_empty() {
                        format!("(stdout: {})", stdout_msg.trim())
                    } else {
                        "No stdout/stderr produced".to_string()
                    };
                    (
                        2, // UNKNOWN/FAILED
                        format!("Subprocess exited with {}: {}", display_code, combined_msg),
                    )
                };

                let mut details = Vec::new();
                if !limit_type.is_empty() {
                    details.push(make_detail_any("limit_type", &limit_type));
                }
                if let Some(sig) = exit_signal {
                    details.push(make_detail_any("exit_signal", &sig.to_string()));
                }
                if exit_code != 0 {
                    details.push(make_detail_any("exit_code", &exit_code.to_string()));
                }
                if sampled_rss > 0 {
                    details.push(make_detail_any("peak_rss_bytes", &sampled_rss.to_string()));
                }
                if sampled_cpu > 0 {
                    details.push(make_detail_any("cpu_time_ms", &sampled_cpu.to_string()));
                }
                details.push(make_detail_any("wall_time_ms", &wall_time_ms.to_string()));

                Some(ErrorStatus {
                    code,
                    message: legible_msg,
                    details,
                    retryable: false,
                })
            };

            let resource_usage = vam_spec::v1::ResourceUsage {
                wall_time_ms,
                cpu_time_ms: sampled_cpu,
                peak_rss_bytes: sampled_rss,
                bytes_written: (stdout_bytes.len() + stderr_bytes.len()) as u64,
                exit_signal: exit_signal.unwrap_or(0),
                limit_exceeded,
                limit_type,
            };

            let duration = start_instant.elapsed();
            record_tool_exec_telemetry(
                tool_name,
                lease_id.as_deref(),
                exit_code,
                duration,
                trace_context,
                &stdout_bytes,
                &stderr_bytes,
                HashMap::new(),
            );

            Ok(SubprocessToolResult {
                stdout: stdout_bytes,
                stderr: stderr_bytes,
                system_logs,
                exit_code,
                error,
                resource_usage: Some(resource_usage),
            })
        }
        _ => {
            // 6. Escalating kill
            if let Some(_p) = pid {
                #[cfg(target_family = "unix")]
                {
                    // Send SIGTERM to process group
                    kill_process_group(_p, libc::SIGTERM);
                    sleep(Duration::from_millis(50)).await;
                    // Escalate to SIGKILL
                    kill_process_group(_p, libc::SIGKILL);
                }
            }

            let wall_time_ms = start_instant.elapsed().as_millis() as u64;
            let (sampled_cpu, sampled_rss) = if let Some(p) = pid {
                sample_proc_metrics(p)
            } else {
                (0, 0)
            };

            let resource_usage = vam_spec::v1::ResourceUsage {
                wall_time_ms,
                cpu_time_ms: sampled_cpu,
                peak_rss_bytes: sampled_rss,
                bytes_written: (stdout_bytes.len() + stderr_bytes.len()) as u64,
                exit_signal: 9, // SIGKILL
                limit_exceeded: true,
                limit_type: "WALL_CLOCK_TIMEOUT".to_string(),
            };

            let details = vec![
                make_detail_any("limit_type", "WALL_CLOCK_TIMEOUT"),
                make_detail_any("max_wall_time_ms", &max_wall_ms.to_string()),
                make_detail_any("wall_time_ms", &wall_time_ms.to_string()),
                make_detail_any("exit_signal", "9"),
                make_detail_any("exit_code", "124"),
            ];

            let error = ErrorStatus {
                code: 8, // RESOURCE_EXHAUSTED
                message: format!(
                    "Resource Limit Exceeded (Timeout): The tool execution exceeded the maximum allocated wall-clock time limit of {}ms.",
                    max_wall_ms
                ),
                details,
                retryable: false,
            };

            let duration = start_instant.elapsed();
            let mut extra = HashMap::new();
            extra.insert("tool.timeout".to_string(), "true".to_string());
            record_tool_exec_telemetry(
                tool_name,
                lease_id.as_deref(),
                124,
                duration,
                trace_context,
                &stdout_bytes,
                &stderr_bytes,
                extra,
            );

            Ok(SubprocessToolResult {
                stdout: stdout_bytes,
                stderr: stderr_bytes,
                system_logs: "[vam-runtime] Subprocess execution exceeded wall-clock timeout and was terminated.\n".to_string(),
                exit_code: 124,
                error: Some(error),
                resource_usage: Some(resource_usage),
            })
        }
    }
}

/// Record stdout/stderr first, then `tool.exec`, so a FIFO flush delivers bodies before the span.
// Each argument is a distinct telemetry field; a params struct would be a refactor.
#[allow(clippy::too_many_arguments)]
fn record_tool_exec_telemetry(
    tool_name: &str,
    lease_id: Option<&str>,
    exit_code: i32,
    duration: Duration,
    trace_context: Option<&vam_spec::v1::TraceContext>,
    stdout: &[u8],
    stderr: &[u8],
    extra_attrs: HashMap<String, String>,
) {
    let mut log_attrs = HashMap::new();
    log_attrs.insert("tool.name".to_string(), tool_name.to_string());
    if let Some(id) = lease_id.filter(|s| !s.is_empty()) {
        log_attrs.insert("lease.id".to_string(), id.to_string());
    }
    if !stdout.is_empty() {
        crate::telemetry::global_telemetry().record_log_with_attrs(
            "stdout",
            stdout.to_vec(),
            trace_context.cloned(),
            log_attrs.clone(),
        );
    }
    if !stderr.is_empty() {
        crate::telemetry::global_telemetry().record_log_with_attrs(
            "stderr",
            stderr.to_vec(),
            trace_context.cloned(),
            log_attrs,
        );
    }

    let mut attrs = extra_attrs;
    attrs.insert("tool.name".to_string(), tool_name.to_string());
    attrs.insert("tool.exit_code".to_string(), exit_code.to_string());
    if let Some(id) = lease_id.filter(|s| !s.is_empty()) {
        attrs.insert("lease.id".to_string(), id.to_string());
    }
    crate::telemetry::global_telemetry().record_span(
        "tool.exec",
        duration,
        trace_context.cloned(),
        attrs,
    );
}

fn make_detail_any(key: &str, value: &str) -> prost_types::Any {
    prost_types::Any {
        type_url: format!("type.googleapis.com/vam.v1.Detail.{}", key),
        value: format!("{}={}", key, value).into_bytes(),
    }
}

fn struct_to_json(s: &prost_types::Struct) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (k, v) in &s.fields {
        if let Some(val) = value_to_json(v) {
            map.insert(k.clone(), val);
        }
    }
    serde_json::Value::Object(map)
}

fn value_to_json(v: &prost_types::Value) -> Option<serde_json::Value> {
    match &v.kind {
        Some(prost_types::value::Kind::NullValue(_)) => Some(serde_json::Value::Null),
        Some(prost_types::value::Kind::NumberValue(n)) => {
            serde_json::Number::from_f64(*n).map(serde_json::Value::Number)
        }
        Some(prost_types::value::Kind::StringValue(s)) => {
            Some(serde_json::Value::String(s.clone()))
        }
        Some(prost_types::value::Kind::BoolValue(b)) => Some(serde_json::Value::Bool(*b)),
        Some(prost_types::value::Kind::StructValue(s)) => Some(struct_to_json(s)),
        Some(prost_types::value::Kind::ListValue(l)) => {
            let mut arr = Vec::new();
            for item in &l.values {
                if let Some(val) = value_to_json(item) {
                    arr.push(val);
                }
            }
            Some(serde_json::Value::Array(arr))
        }
        None => None,
    }
}

/// The `RLIMIT_NPROC` to apply to a tool process, or `None` when the build must not
/// apply one. A `process-guest` build runs the guest as a host process, where the
/// limit counts every thread of that host UID; default and VM builds keep it.
#[cfg(target_os = "linux")]
fn nproc_limit(max_procs: u64) -> Option<libc::rlimit> {
    if cfg!(feature = "process-guest") {
        None
    } else {
        Some(libc::rlimit {
            rlim_cur: max_procs,
            rlim_max: max_procs,
        })
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn nproc_limit_is_applied_in_default_builds_and_skipped_in_process_guest() {
        let limit = super::nproc_limit(64);
        if cfg!(feature = "process-guest") {
            assert!(limit.is_none());
        } else {
            let limit = limit.expect("default builds keep the limit");
            assert_eq!((limit.rlim_cur, limit.rlim_max), (64, 64));
        }
    }

    /// Serializes the tests that drain the process-wide telemetry buffer, so
    /// one test's drain cannot take another test's frames mid-check.
    fn telemetry_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap()
    }

    use super::*;

    /// Parent directories of files `scripts/build-rootfs.sh` marks executable.
    ///
    /// `/sbin/init` is the guest PID 1 image, not a command looked up on
    /// `PATH`, so that one install is not a PATH directory.
    #[cfg(unix)]
    fn rootfs_executable_dirs(script: &str) -> Vec<String> {
        let mut dirs = std::collections::BTreeSet::new();
        for line in script.lines() {
            let Some(rest) = line.trim().strip_prefix("chmod +x ") else {
                continue;
            };
            let path = rest.trim().trim_matches('"');
            let Some(rel) = path.strip_prefix("$STAGING_DIR/") else {
                continue;
            };
            if rel == "sbin/init" {
                continue;
            }
            let Some(parent) = std::path::Path::new(rel).parent() else {
                continue;
            };
            if parent.as_os_str().is_empty() {
                continue;
            }
            dirs.insert(format!("/{}", parent.display()));
        }
        dirs.into_iter().collect()
    }

    #[cfg(unix)]
    #[test]
    fn guest_path_covers_rootfs_executable_dirs() {
        let script_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/build-rootfs.sh");
        let script = std::fs::read_to_string(&script_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", script_path.display()));
        let dirs = rootfs_executable_dirs(&script);
        assert!(
            dirs.iter().any(|dir| dir == "/usr/local/bin"),
            "build-rootfs.sh no longer installs an executable under /usr/local/bin; parser missed bun"
        );
        let path_dirs: Vec<&str> = GUEST_TOOL_PATH.split(':').collect();
        for dir in &dirs {
            assert!(
                path_dirs.contains(&dir.as_str()),
                "injected PATH `{GUEST_TOOL_PATH}` is missing `{dir}`, which scripts/build-rootfs.sh installs an executable into"
            );
        }
    }

    #[test]
    fn test_sample_proc_metrics_self() {
        let pid = std::process::id();
        let (cpu, rss) = sample_proc_metrics(pid);
        #[cfg(target_os = "linux")]
        {
            // Current test process has non-zero RSS
            assert!(rss > 0, "Current process RSS must be greater than 0");
        }
        // CPU time should be non-negative
        println!(
            "Sampled self metrics: cpu_time_ms={}, peak_rss_bytes={}",
            cpu, rss
        );
    }

    #[tokio::test]
    async fn test_execute_tool_subprocess_resource_usage() {
        let mut args = prost_types::Struct::default();
        args.fields.insert(
            "args".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::ListValue(
                    prost_types::ListValue {
                        values: vec![prost_types::Value {
                            kind: Some(prost_types::value::Kind::StringValue(
                                "hello-resource-metering".to_string(),
                            )),
                        }],
                    },
                )),
            },
        );

        let res = execute_tool_subprocess("echo", Some(&args), Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(res.exit_code, 0);
        let stdout_str = String::from_utf8_lossy(&res.stdout);
        assert!(stdout_str.contains("hello-resource-metering"));

        assert!(
            res.resource_usage.is_some(),
            "Must return resource usage telemetry"
        );
        let usage = res.resource_usage.unwrap();
        assert!(usage.wall_time_ms > 0 || usage.bytes_written > 0);
        assert!(usage.bytes_written > 0);
        assert_eq!(usage.exit_signal, 0);
        assert!(!usage.limit_exceeded);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_s3_timeout_yields_legible_result_and_usage() {
        let mut args = prost_types::Struct::default();
        args.fields.insert(
            "args".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::ListValue(
                    prost_types::ListValue {
                        values: vec![prost_types::Value {
                            kind: Some(prost_types::value::Kind::StringValue(
                                "sleep 2".to_string(),
                            )),
                        }],
                    },
                )),
            },
        );

        // Subprocess timeout configured to 150ms
        let res = execute_tool_subprocess("sh", Some(&args), Duration::from_millis(150), None)
            .await
            .unwrap();
        assert_eq!(res.exit_code, 124, "Timeout must return exit code 124");
        assert!(
            res.error.is_some(),
            "Must return legible ErrorStatus on timeout"
        );

        let err = res.error.unwrap();
        assert_eq!(err.code, 8, "RESOURCE_EXHAUSTED error code");
        assert!(err.message.contains("Resource Limit Exceeded (Timeout)"));
        assert!(err
            .details
            .iter()
            .any(|d| String::from_utf8_lossy(&d.value).contains("limit_type=WALL_CLOCK_TIMEOUT")));

        assert!(
            res.resource_usage.is_some(),
            "ResourceUsage must NOT be lost on timeout"
        );
        let usage = res.resource_usage.unwrap();
        assert!(usage.limit_exceeded);
        assert_eq!(usage.limit_type, "WALL_CLOCK_TIMEOUT");
        assert_eq!(usage.exit_signal, 9);
    }

    #[test]
    fn test_s3_script_digest_from_file() {
        let script_file =
            std::env::temp_dir().join(format!("test_s3_tool_{}.py", std::process::id()));
        let content = b"print('hello digest test')";
        std::fs::write(&script_file, content).unwrap();

        let mut args = prost_types::Struct::default();
        args.fields.insert(
            "file".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    script_file.to_str().unwrap().to_string(),
                )),
            },
        );

        let (digest, path_opt) = compute_script_digest("custom", Some(&args));
        assert_eq!(path_opt, Some(script_file.to_str().unwrap().to_string()));
        let expected_digest = format!("{:x}", Sha256::digest(content));
        assert_eq!(digest, expected_digest);

        let _ = std::fs::remove_file(&script_file);
    }

    #[tokio::test]
    async fn test_run_builtin_delegates_to_sh() {
        let mut args = prost_types::Struct::default();
        args.fields.insert(
            "command".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "echo hello-run-builtin".to_string(),
                )),
            },
        );

        let res = execute_tool_subprocess("run", Some(&args), Duration::from_secs(5), None)
            .await
            .unwrap();
        assert_eq!(res.exit_code, 0);
        assert!(String::from_utf8_lossy(&res.stdout).contains("hello-run-builtin"));
    }

    #[tokio::test]
    // Test-only lock that serialises the global telemetry buffer; held on purpose.
    #[allow(clippy::await_holding_lock)]
    async fn test_phase_4_4_traceparent_injection() {
        let _guard = telemetry_test_guard();
        use crate::telemetry::global_telemetry;
        use vam_spec::v1::TraceContext;

        let tc = TraceContext {
            traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_string(),
            tracestate: "rojo=1".to_string(),
        };

        #[cfg(unix)]
        let cmd_str = "echo $OTEL_TRACEPARENT";
        #[cfg(windows)]
        let cmd_str = "echo %OTEL_TRACEPARENT%";

        let mut args = prost_types::Struct::default();
        args.fields.insert(
            "command".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(cmd_str.to_string())),
            },
        );

        let res = execute_tool_subprocess_with_trace(
            "run",
            Some(&args),
            Duration::from_secs(5),
            None,
            Some(&tc),
        )
        .await
        .unwrap();

        assert_eq!(res.exit_code, 0);
        let out = String::from_utf8_lossy(&res.stdout);
        assert!(
            out.contains("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
            "Expected stdout to contain injected traceparent, got: {}",
            out
        );

        // Verify that a tool.exec span was recorded into the global buffer
        let frames = global_telemetry().drain_batch(5000);
        assert!(
            frames.iter().any(|f| f.span_name == "tool.exec"
                && f.attributes.get("tool.name") == Some(&"run".to_string())),
            "Expected tool.exec span in guest telemetry buffer"
        );
    }

    #[tokio::test]
    // Test-only lock that serialises the global telemetry buffer; held on purpose.
    #[allow(clippy::await_holding_lock)]
    async fn test_exec_telemetry_logs_before_span_with_attrs() {
        use crate::telemetry::global_telemetry;

        let _guard = telemetry_test_guard();
        let _ = global_telemetry().drain_batch(5000);

        let mut args = prost_types::Struct::default();
        args.fields.insert(
            "command".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "printf 'hello-stdout\\n'; printf 'err-line\\n' >&2; exit 2".to_string(),
                )),
            },
        );

        let res = execute_tool_subprocess(
            "run",
            Some(&args),
            Duration::from_secs(5),
            Some("lease-fidelity-1".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(res.exit_code, 2);

        let frames = global_telemetry().drain_batch(5000);
        // Other tests in this process also record telemetry, so match on this
        // run's lease id rather than the first frame of each kind.
        let this_run = |f: &&vam_spec::v1::LogFrame| {
            f.attributes.get("lease.id").map(String::as_str) == Some("lease-fidelity-1")
        };
        let stdout = frames
            .iter()
            .filter(this_run)
            .find(|f| f.channel == "stdout")
            .expect("stdout log");
        let stderr = frames
            .iter()
            .filter(this_run)
            .find(|f| f.channel == "stderr")
            .expect("stderr log");
        let span = frames
            .iter()
            .filter(this_run)
            .find(|f| f.span_name == "tool.exec")
            .expect("tool.exec span");

        assert_eq!(
            stdout.attributes.get("tool.name").map(String::as_str),
            Some("run")
        );
        assert_eq!(
            stdout.attributes.get("lease.id").map(String::as_str),
            Some("lease-fidelity-1")
        );
        assert_eq!(
            stderr.attributes.get("tool.name").map(String::as_str),
            Some("run")
        );
        assert_eq!(
            stderr.attributes.get("lease.id").map(String::as_str),
            Some("lease-fidelity-1")
        );
        assert_eq!(
            span.attributes.get("tool.exit_code").map(String::as_str),
            Some("2")
        );
        assert!(
            stdout.sequence_number < span.sequence_number,
            "stdout must flush before tool.exec"
        );
        assert!(
            stderr.sequence_number < span.sequence_number,
            "stderr must flush before tool.exec"
        );
    }

    #[test]
    fn skill_entrypoint_resolves_from_relocated_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let skill_dir = root.path().join("acme--jira");
        std::fs::create_dir_all(&skill_dir).expect("mkdir");
        std::fs::write(skill_dir.join("execute.js"), b"console.log('relocated')").expect("script");
        let manifest = r#"{
            "tools": [{
                "name": "jira_search_issues",
                "localExec": {
                    "command": "bun",
                    "entrypoint": "./execute.js",
                    "defaultArgs": ["search"]
                }
            }]
        }"#;
        std::fs::write(skill_dir.join("manifest.json"), manifest).expect("manifest");

        let (cmd, args) = ToolRegistry::resolve_dynamic_at("jira_search_issues", root.path())
            .expect("relocated skill resolves");
        assert_eq!(cmd, "/usr/local/bin/bun");
        assert_eq!(
            args,
            vec![
                skill_dir
                    .join("execute.js")
                    .to_str()
                    .expect("utf8")
                    .to_string(),
                "search".to_string(),
            ]
        );
        assert!(!args[0].contains("/mnt/drive9/skills/jira-integration"));

        let baked = manifest.replace(
            "./execute.js",
            "/mnt/drive9/skills/jira-integration/execute.js",
        );
        std::fs::write(skill_dir.join("manifest.json"), baked).expect("baked manifest");
        assert!(
            ToolRegistry::resolve_dynamic_at("jira_search_issues", root.path()).is_none(),
            "an absolute mount path does not follow the skill when its directory changes"
        );
    }
}
