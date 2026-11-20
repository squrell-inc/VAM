//! `vam connector test`: run a listing's tools against the mock its test file declares, offline.
//!
//! For each case of `listings/<id>/tests/connector-test.json` the tool's entrypoint is run as the
//! guest runtime runs it (`vam-runtime/src/exec.rs`): a cleared environment with `PATH` kept, a
//! scratch `HOME` and workspace, `VAM_EGRESS_ENDPOINT` on a loopback stub, the tool's input on
//! stdin, and `.vam/connector.json` next to the skill's manifest when the connector has a company
//! address. The skill is copied into the scratch directory first, so a tool cannot change the
//! developer's tree.
//!
//! The stub stands where the node's egress gate stands. It reads each request's `X-Target-Url`,
//! screens it with the gate's own code (`vam_spec::connection_envelope::Envelope::screen`, the
//! tool's declared shapes, the connector's bindings), puts a generated credential (`vamkey_` and 32
//! hex digits; `vamtok_` for an OAuth connector's access token) in the request only after the gate
//! admitted it, answers from the test file's routes (`ConnectorTestFile::answer_for`, in process,
//! nothing leaves the machine) and scrubs the credential from the answer. A refusal reaches the tool
//! as `502 Bad Gateway: <the gate's words>`, as the guest proxy reports one.
//!
//! A case then passes or fails. The credential must be in no tool output and no file the tool left
//! in the scratch directory (the check comes first, and its message never quotes the value).
//!
//! The tool's code runs on this machine with a cleared environment and no sandbox: it is the
//! publisher's own code. A connector that is a remote MCP server has nothing to run here (the node
//! is its client): only the test file is checked.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};
use vam_security::secret_forms::SecretRedactor;
use vam_spec::connection_envelope::{
    inject_header, may_carry_placeholder, ActiveCall, BindingSpec, CallWindow, ConnectionRequest,
    Envelope, HeaderInject, Shape,
};
use vam_spec::connector::{
    check_subdomain, fill_subdomain, templated_suffix, BindingValue, Connector,
};
use vam_spec::connector_kit::{ConnectorTestFile, Expect, MockAnswer, TestCase, TEST_FILE_PATH};
use vam_spec::marketplace::{parse_index, resolve_artifact_dir, INDEX_FILE};
use vam_spec::package::{check_dir, read_package_file, ArtifactKind, CheckedSpec};
use vam_spec::v1::{tool_spec::ExecutionProvider, SkillManifest, ToolSpec};

use crate::output::{self, fail, ExitCode};

/// How long a tool may run when its manifest names no timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest timeout a manifest can ask for here.
const MAX_TIMEOUT: Duration = Duration::from_secs(120);
/// The most of a tool's output that is kept.
const MAX_OUTPUT: usize = 2 * 1024 * 1024;
/// The largest file scanned for the credential.
const MAX_SCAN_FILE: u64 = 16 * 1024 * 1024;
/// The largest request the stub reads.
const MAX_REQUEST: usize = 8 * 1024 * 1024;
/// The sample company address when neither `--subdomain` nor the test file names one.
const DEFAULT_SUBDOMAIN: &str = "demo";

/// What `vam connector test` printed for one case or listing.
struct Line {
    listing: String,
    tool: String,
    failure: Option<String>,
}

/// `vam connector test <dir> [--listing <id>] [--subdomain <label>]`.
pub fn test(dir: &str, listing: Option<&str>, subdomain: Option<&str>) -> Result<()> {
    let subdomain = subdomain
        .map(|s| check_subdomain(s).map_err(|e| fail(ExitCode::Usage, e.to_string())))
        .transpose()?;
    let (root, listings) = locate(dir, listing)?;
    let mut lines: Vec<Line> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for id in &listings {
        test_listing(
            &root,
            id,
            subdomain.as_deref(),
            listing.is_some(),
            &mut |mut l| {
                // One line per case: a message that spans lines is joined.
                l.failure = l
                    .failure
                    .map(|w| w.split_whitespace().collect::<Vec<_>>().join(" "));
                match &l.failure {
                    None => human(|| println!("pass {}", l.tool)),
                    Some(why) => human(|| println!("fail {}: {why}", l.tool)),
                }
                lines.push(l);
            },
            &mut |s| {
                human(|| println!("skip {s}"));
                skipped.push(s);
            },
        )?;
    }
    let failed = lines.iter().filter(|l| l.failure.is_some()).count();
    let summary = format!("{} case(s) passed, {failed} failed", lines.len() - failed);
    if output::is_json() {
        let cases: Vec<Value> = lines
            .iter()
            .map(|l| {
                json!({
                    "listing": l.listing,
                    "tool": l.tool,
                    "passed": l.failure.is_none(),
                    "message": l.failure,
                })
            })
            .collect();
        if failed > 0 {
            let why: Vec<String> = lines
                .iter()
                .filter_map(|l| l.failure.as_ref().map(|w| format!("{}: {w}", l.tool)))
                .collect();
            return Err(fail(
                ExitCode::Invalid,
                format!("{summary}; {}", why.join("; ")),
            ));
        }
        println!(
            "{}",
            json!({ "ok": true, "cases": cases, "skipped": skipped })
        );
        return Ok(());
    }
    println!("{summary}");
    if failed > 0 {
        return Err(fail(
            ExitCode::Invalid,
            format!("{failed} of {} case(s) failed", lines.len()),
        ));
    }
    println!("the key never reached a tool");
    Ok(())
}

fn human(print: impl FnOnce()) {
    if !output::is_json() {
        print();
    }
}

/// The marketplace root and the listings to test: every listing of a root, `--listing`, or the one
/// listing directory given.
fn locate(dir: &str, only: Option<&str>) -> Result<(PathBuf, Vec<String>)> {
    let path = Path::new(dir);
    if !path.join(INDEX_FILE).exists() {
        let (root, id) = super::locate_listing(path, dir)?;
        if only.is_some_and(|o| o != id) {
            return Err(fail(
                ExitCode::Usage,
                format!("`{dir}` is the listing `{id}`, not the listing named by --listing"),
            ));
        }
        return Ok((root, vec![id]));
    }
    let raw = read_package_file(&path.join(INDEX_FILE))
        .map_err(|e| fail(ExitCode::Invalid, e.to_string()))?;
    let index = parse_index(&raw).map_err(|problems| {
        let lines: Vec<String> = problems.iter().map(|p| p.message.clone()).collect();
        fail(
            ExitCode::Invalid,
            format!(
                "the marketplace index is not valid (`vam connector validate` names it): {}",
                lines.join("; ")
            ),
        )
    })?;
    let ids: Vec<String> = index.listings.iter().map(|l| l.id.clone()).collect();
    match only {
        Some(id) if !ids.iter().any(|i| i == id) => Err(fail(
            ExitCode::Usage,
            format!("the marketplace index names no listing `{id}`"),
        )),
        Some(id) => Ok((path.to_path_buf(), vec![id.to_string()])),
        None => Ok((path.to_path_buf(), ids)),
    }
}

/// What one listing ships that `test` needs.
struct Listing {
    connector: Connector,
    skills: Vec<(PathBuf, SkillManifest)>,
}

fn test_listing(
    root: &Path,
    id: &str,
    subdomain: Option<&str>,
    explicit: bool,
    report: &mut dyn FnMut(Line),
    skip: &mut dyn FnMut(String),
) -> Result<()> {
    let listing_fail = |report: &mut dyn FnMut(Line), why: String| {
        report(Line {
            listing: id.to_string(),
            tool: id.to_string(),
            failure: Some(why),
        })
    };
    let file_path = root.join("listings").join(id).join(TEST_FILE_PATH);
    if fs::symlink_metadata(&file_path).is_err() {
        if explicit {
            listing_fail(
                report,
                format!("listings/{id}/{TEST_FILE_PATH} is missing: there is nothing to run"),
            );
        } else {
            skip(format!("{id}: no listings/{id}/{TEST_FILE_PATH}"));
        }
        return Ok(());
    }
    let file = match read_package_file(&file_path)
        .map_err(|e| e.to_string())
        .and_then(|t| ConnectorTestFile::parse(&t).map_err(|e| e.to_string()))
    {
        Ok(f) => f,
        Err(why) => {
            listing_fail(report, why);
            return Ok(());
        }
    };
    let shipped = match read_listing(root, id) {
        Ok(l) => l,
        Err(why) => {
            listing_fail(report, why);
            return Ok(());
        }
    };
    let Some(shipped) = shipped else {
        listing_fail(
            report,
            "the listing ships no connector, so a tool's shapes have nothing to be checked against"
                .into(),
        );
        return Ok(());
    };
    if shipped.connector.mcp.is_some() {
        skip(format!(
            "{id}: a remote MCP connector (the node is its client): nothing runs here; the test file is valid ({} route(s), {} case(s))",
            file.routes.len(),
            file.cases.len()
        ));
        return Ok(());
    }
    let world = match World::new(&shipped, &file, subdomain) {
        Ok(w) => w,
        Err(why) => {
            listing_fail(report, why);
            return Ok(());
        }
    };
    for (n, case) in file.cases.iter().enumerate() {
        let failure = world.run_case(&shipped, &file, case, n).err();
        report(Line {
            listing: id.to_string(),
            tool: case.tool.clone(),
            failure,
        });
    }
    Ok(())
}

/// The listing's connector and skills, each read with the package check of its kind. `Ok(None)`
/// when it ships no connector.
fn read_listing(root: &Path, id: &str) -> Result<Option<Listing>, String> {
    let raw = read_package_file(&root.join(INDEX_FILE)).map_err(|e| e.to_string())?;
    let index = parse_index(&raw).map_err(|_| "the marketplace index is not valid".to_string())?;
    let entry = index
        .listings
        .iter()
        .find(|l| l.id == id)
        .ok_or_else(|| format!("the marketplace index names no listing `{id}`"))?;
    let mut connector = None;
    let mut skills = Vec::new();
    for a in &entry.artifacts {
        if !matches!(a.kind, ArtifactKind::Connector | ArtifactKind::Skill) {
            continue;
        }
        let dir = resolve_artifact_dir(root, &a.path)
            .map_err(|why| format!("artifact path `{}` {why}", a.path))?;
        let checked = check_dir(&dir, Some(a.kind)).map_err(|e| format!("{}: {}", a.path, e.0))?;
        match checked.spec {
            CheckedSpec::Connector(_) if connector.is_some() => {
                return Err(
                    "the listing ships more than one connector: `vam connector test` takes a listing with one"
                        .into(),
                )
            }
            CheckedSpec::Connector(c) => connector = Some(*c),
            CheckedSpec::Skill(m) => skills.push((dir, m)),
            _ => {}
        }
    }
    Ok(connector.map(|connector| Listing { connector, skills }))
}

/// What the cases of one listing share: the connector with the sample company address filled.
struct World {
    connector: Connector,
    bindings: Vec<BindingSpec>,
    /// The sample address, for a connector with one.
    address: Option<(String, String)>,
    credential_prefix: &'static str,
}

impl World {
    fn new(
        shipped: &Listing,
        file: &ConnectorTestFile,
        subdomain: Option<&str>,
    ) -> Result<Self, String> {
        let c = &shipped.connector;
        let address = match (&c.tenant, subdomain) {
            (None, Some(_)) => {
                return Err(
                    "--subdomain is for a connector with a company address; this one has none"
                        .into(),
                )
            }
            (None, None) => None,
            (Some(t), Some(s)) => Some((s.to_string(), t.suffix.clone())),
            (Some(t), None) => {
                // The test file's own host tells the address it was written for.
                let suffix = format!(".{}", t.suffix);
                let label = file
                    .hosts
                    .iter()
                    .find_map(|h| h.strip_suffix(&suffix).filter(|l| !l.contains('.')))
                    .unwrap_or(DEFAULT_SUBDOMAIN);
                Some((label.to_string(), t.suffix.clone()))
            }
        };
        let filled = fill_subdomain(c, address.as_ref().map(|(s, _)| s.as_str()));
        let bindings: Vec<BindingSpec> = filled
            .bindings
            .iter()
            .flatten()
            .map(|b| BindingSpec {
                connector: filled.urn.clone(),
                name: b.name.clone(),
                placeholder: b.placeholder.clone(),
                hosts: b.hosts.clone(),
                header: HeaderInject::of(b),
            })
            .collect();
        if bindings.is_empty() {
            return Err(
                "the connector declares no binding, so no tool can reach it through a credential"
                    .into(),
            );
        }
        let credential_prefix = match filled.bindings.iter().flatten().next().map(|b| b.value) {
            Some(BindingValue::AccessToken) => "vamtok_",
            _ => "vamkey_",
        };
        Ok(Self {
            connector: filled,
            bindings,
            address,
            credential_prefix,
        })
    }

    /// One case: run the tool behind the stub and judge it.
    fn run_case(
        &self,
        shipped: &Listing,
        file: &ConnectorTestFile,
        case: &TestCase,
        n: usize,
    ) -> Result<(), String> {
        let tool = self.find_tool(shipped, &case.tool)?;
        let credential = format!(
            "{}{}",
            self.credential_prefix,
            uuid::Uuid::new_v4().simple()
        );
        let scratch = tempfile::tempdir().map_err(|e| format!("no scratch directory: {e}"))?;
        let skill_dir = self.stage(scratch.path(), &tool)?;

        let window = Arc::new(CallWindow::new());
        window.open(ActiveCall::new(
            format!("connector-test-{n}"),
            tool.name.clone(),
            Some(tool.requires.clone()).filter(|r| !r.is_empty()),
            tool.shapes.clone(),
            tool.timeout + Duration::from_secs(30),
        ));
        let stub = Arc::new(Stub {
            envelope: Envelope::new(self.bindings.clone(), window),
            file: file.clone(),
            credential: credential.clone(),
            events: Mutex::new(Vec::new()),
        });
        let server = Server::start(stub.clone()).map_err(|e| format!("no loopback stub: {e}"))?;
        let ran = run_tool(&tool, &skill_dir, scratch.path(), &case.input, server.port);
        server.stop();
        let events = stub
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let redactor = SecretRedactor::with_replacement(&[credential.as_bytes()], "[credential]");
        if let Ok(out) = &ran {
            if holds(&redactor, &out.stdout) || holds(&redactor, &out.stderr) {
                return Err("the credential reached the tool's output".into());
            }
        }
        if let Some(path) = file_holding(scratch.path(), scratch.path(), &redactor) {
            return Err(format!("the credential was written to a file ({path})"));
        }
        judge(case, &events, ran)
    }

    fn find_tool(&self, shipped: &Listing, name: &str) -> Result<ToolRun, String> {
        for (dir, manifest) in &shipped.skills {
            let Some(tool) = manifest.tools.iter().find(|t| t.name == name) else {
                continue;
            };
            return self.tool_run(dir, manifest, tool);
        }
        Err(format!("the listing's skills have no tool named `{name}`"))
    }

    fn tool_run(
        &self,
        dir: &Path,
        manifest: &SkillManifest,
        tool: &ToolSpec,
    ) -> Result<ToolRun, String> {
        let Some(ExecutionProvider::LocalExec(exec)) = &tool.execution_provider else {
            return Err("the tool has no localExec, so there is nothing to run here".into());
        };
        let command = match exec.command.as_str() {
            "bun" => "bun",
            "node" => "node",
            "python" | "python3" => "python3",
            other => {
                return Err(format!(
                    "the tool's command `{other}` is not one of bun, node and python3"
                ))
            }
        };
        let timeout = tool
            .timeout
            .as_ref()
            .and_then(|d| u64::try_from(d.seconds).ok().map(Duration::from_secs))
            .filter(|d| !d.is_zero())
            .map_or(DEFAULT_TIMEOUT, |d| d.min(MAX_TIMEOUT));
        let skill_name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("skill")
            .to_string();
        Ok(ToolRun {
            name: tool.name.clone(),
            requires: manifest.requires_connector.clone(),
            shapes: tool
                .egress
                .iter()
                .filter_map(|e| self.fill_shape(e))
                .collect(),
            command,
            entrypoint: exec.entrypoint.clone(),
            default_args: exec.default_args.clone(),
            timeout,
            source: dir.to_path_buf(),
            skill_name,
        })
    }

    /// A shape as the node fills it: a company address host takes the sample address, and one
    /// under another suffix is dropped (nothing is admitted for it).
    fn fill_shape(&self, e: &vam_spec::v1::ToolEgress) -> Option<Shape> {
        let mut host = e.host.clone();
        if host.contains(['{', '}']) {
            let want = templated_suffix(&host)?;
            let (sub, suffix) = self.address.as_ref()?;
            if want != suffix {
                return None;
            }
            host = format!("{sub}.{suffix}");
        }
        Some(Shape {
            binding: e.binding.clone(),
            host,
            methods: e.methods.clone(),
            path_prefixes: e.path_prefixes.clone(),
            paths: e.paths.clone(),
        })
    }

    /// Copies the skill into the scratch directory and writes the hint file next to its manifest
    /// when the connector has a company address, as the node does for a staged skill.
    fn stage(&self, scratch: &Path, tool: &ToolRun) -> Result<PathBuf, String> {
        for d in ["home", "work", "tmp"] {
            fs::create_dir_all(scratch.join(d)).map_err(|e| format!("scratch directory: {e}"))?;
        }
        let dir = scratch.join("skills").join(&tool.skill_name);
        copy_tree(&tool.source, &dir).map_err(|e| format!("cannot stage the skill: {e}"))?;
        if self.address.is_some() {
            let hosts: serde_json::Map<String, Value> = self
                .connector
                .bindings
                .iter()
                .flatten()
                .filter_map(|b| {
                    let h = b.hosts.iter().find(|h| !h.contains(['{', '}']))?;
                    Some((b.name.clone(), Value::String(h.clone())))
                })
                .collect();
            let hint = json!({ "connector": connector_id(&self.connector), "hosts": hosts });
            let hint_dir = dir.join(".vam");
            let _ = fs::remove_dir_all(&hint_dir);
            fs::create_dir_all(&hint_dir)
                .and_then(|()| fs::write(hint_dir.join("connector.json"), hint.to_string()))
                .map_err(|e| format!("cannot write .vam/connector.json: {e}"))?;
        }
        Ok(dir)
    }
}

/// The last segment of the connector's URN.
fn connector_id(c: &Connector) -> String {
    c.urn.rsplit('/').next().unwrap_or_default().to_string()
}

struct ToolRun {
    name: String,
    requires: String,
    shapes: Vec<Shape>,
    command: &'static str,
    entrypoint: String,
    default_args: Vec<String>,
    timeout: Duration,
    source: PathBuf,
    skill_name: String,
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)?;
        }
        // A symlink is not followed or copied.
    }
    Ok(())
}

// ---- running the tool ---------------------------------------------------------------------------

struct Ran {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// `None` when the process was killed at the timeout.
    status: Option<i32>,
}

fn run_tool(
    tool: &ToolRun,
    skill_dir: &Path,
    scratch: &Path,
    input: &Value,
    port: u16,
) -> Result<Ran, String> {
    let entry = vam_spec::resolve_manifest_relative_entrypoint(skill_dir, &tool.entrypoint)
        .map_err(|e| e.to_string())?;
    let endpoint = format!("http://127.0.0.1:{port}");
    let mut cmd = Command::new(tool.command);
    cmd.arg(&entry).args(&tool.default_args);
    cmd.env_clear();
    cmd.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        cmd.env("SystemRoot", root);
    }
    cmd.env("HOME", scratch.join("home"));
    cmd.env("LANG", "en_US.UTF-8");
    cmd.env("TMPDIR", scratch.join("tmp"));
    // Bun and node ignore NO_PROXY and deadlock behind an env proxy: they read
    // VAM_EGRESS_ENDPOINT and name the target in X-Target-Url (as in the guest).
    if matches!(tool.command, "bun" | "node") {
        for k in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            cmd.env(k, "");
        }
        cmd.env("NO_PROXY", "*").env("no_proxy", "*");
    } else {
        for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            cmd.env(k, &endpoint);
        }
        for k in ["NO_PROXY", "no_proxy"] {
            cmd.env(k, "127.0.0.1,localhost");
        }
    }
    cmd.env("VAM_EGRESS_ENDPOINT", &endpoint);
    cmd.current_dir(scratch.join("work"));
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start `{}`: {e}", tool.command))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A tool that does not read its input closes the pipe: not an error.
        let _ = stdin.write_all(input.to_string().as_bytes());
    }
    let drain = |stream: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(s) = stream {
                let _ = s.take(MAX_OUTPUT as u64).read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|s| Box::new(s) as _));
    let err = drain(child.stderr.take().map(|s| Box::new(s) as _));
    let deadline = Instant::now() + tool.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s.code().unwrap_or(-1)),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("cannot wait for the tool: {e}")),
        }
    };
    if status.is_none() {
        // A grandchild may hold the pipes open: the readers are left to end on their own.
        return Ok(Ran {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status,
        });
    }
    Ok(Ran {
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
        status,
    })
}

/// Whether `bytes` hold the credential in any form the redactor knows.
fn holds(redactor: &SecretRedactor, bytes: &[u8]) -> bool {
    matches!(redactor.redact_bytes(bytes), std::borrow::Cow::Owned(_))
}

/// Walks `dir` for a file that holds the credential; the path is relative to `base`.
fn file_holding(base: &Path, dir: &Path, redactor: &SecretRedactor) -> Option<String> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            if let Some(found) = file_holding(base, &path, redactor) {
                return Some(found);
            }
        } else if kind.is_file() && entry.metadata().is_ok_and(|m| m.len() <= MAX_SCAN_FILE) {
            if let Ok(bytes) = fs::read(&path) {
                if holds(redactor, &bytes) {
                    let rel = path.strip_prefix(base).unwrap_or(&path);
                    return Some(rel.display().to_string());
                }
            }
        }
    }
    None
}

// ---- judging a case -----------------------------------------------------------------------------

/// What the stub saw of a case.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    /// The gate refused a request (its words).
    Refused(String),
    /// A request was answered with this status.
    Answered(u16),
    /// A request that could not be answered here (the stub's words).
    Unanswered(String),
}

/// `Ok` when the case holds; `Err` is the line after `fail <tool>: `.
fn judge(case: &TestCase, events: &[Event], ran: Result<Ran, String>) -> Result<(), String> {
    let refusal = events.iter().find_map(|e| match e {
        Event::Refused(text) => Some(text.as_str()),
        _ => None,
    });
    match case.expect {
        Expect::Refused => match refusal {
            Some(text) => match case.contains.as_deref() {
                Some(want) if !text.contains(want) => {
                    Err(format!("the refusal does not say `{want}`: {text}"))
                }
                _ => Ok(()),
            },
            None => Err(match events.iter().find_map(|e| match e {
                Event::Answered(status) => Some(*status),
                _ => None,
            }) {
                Some(status) => format!(
                    "expected the gate to refuse the tool's request, but it was admitted and answered {status}"
                ),
                None => "expected the gate to refuse the tool's request, but the tool made none"
                    .to_string(),
            }),
        },
        Expect::Ok => {
            if let Some(text) = refusal {
                return Err(text.to_string());
            }
            if let Some(why) = events.iter().find_map(|e| match e {
                Event::Unanswered(why) => Some(why.as_str()),
                _ => None,
            }) {
                return Err(why.to_string());
            }
            let ran = ran?;
            let Some(code) = ran.status else {
                return Err("the tool did not finish in time".into());
            };
            if code != 0 {
                return Err(format!(
                    "the tool exited with status {code}{}",
                    last_line(&ran.stderr)
                        .map(|l| format!(": {l}"))
                        .unwrap_or_default()
                ));
            }
            match case.contains.as_deref() {
                Some(want) if !String::from_utf8_lossy(&ran.stdout).contains(want) => {
                    Err(format!("the tool's output does not contain `{want}`"))
                }
                _ => Ok(()),
            }
        }
    }
}

/// The last non-empty line of a stream, on one line and short.
fn last_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
    let mut line: String = line.chars().filter(|c| !c.is_control()).collect();
    if line.len() > 160 {
        let mut end = 159;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push('…');
    }
    Some(line)
}

// ---- the stub -----------------------------------------------------------------------------------

/// A request as the tool sent it to the stub.
struct RawRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// What the stub sends back.
struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

/// Where the node's egress gate stands, for one case.
struct Stub {
    envelope: Envelope,
    file: ConnectorTestFile,
    credential: String,
    events: Mutex<Vec<Event>>,
}

impl Stub {
    fn log(&self, e: Event) {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(e);
    }

    fn bad_gateway(&self, why: &str) -> Reply {
        Reply {
            status: 502,
            content_type: "text/plain",
            body: format!("Bad Gateway: {why}").into_bytes(),
        }
    }

    /// The gate's decision and the mock's answer for one request.
    fn handle(&self, req: &RawRequest) -> Reply {
        // The destination is X-Target-Url, else an absolute request target, else Host and the
        // target, as the guest proxy reads it; neither header goes on to the gate.
        let header = |name: &str| {
            req.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        };
        let url = match header("x-target-url") {
            Some(u) => u,
            None if req.target.starts_with("http://") || req.target.starts_with("https://") => {
                req.target.clone()
            }
            None => match header("host") {
                Some(h) => format!("http://{h}{}", req.target),
                None => return self.bad_gateway("Missing Host header or absolute URI"),
            },
        };
        let mut headers: HashMap<String, String> = req
            .headers
            .iter()
            .filter(|(k, _)| {
                !k.eq_ignore_ascii_case("x-target-url") && !k.eq_ignore_ascii_case("host")
            })
            .cloned()
            .collect();
        let request = ConnectionRequest {
            method: &req.method,
            url: &url,
            headers: &headers,
            body: &req.body,
        };
        match self.envelope.screen(&request) {
            Err(refusal) => {
                self.log(Event::Refused(refusal.text.clone()));
                self.bad_gateway(&refusal.text)
            }
            Ok(Some(grant)) => {
                // Only an admitted request gets the credential, put where the gate puts it: the
                // one header the binding names, or `Authorization: Bearer`.
                let header = self
                    .envelope
                    .binding(&grant.connector, &grant.binding)
                    .and_then(|b| b.header.clone());
                match &header {
                    None => {
                        headers.retain(|k, _| !k.eq_ignore_ascii_case("authorization"));
                        headers.insert(
                            "Authorization".into(),
                            format!("Bearer {}", self.credential),
                        );
                    }
                    Some(h) => {
                        if let Err(refusal) =
                            inject_header(&mut headers, &h.name, &h.prefix, &self.credential)
                        {
                            self.log(Event::Refused(refusal.text.clone()));
                            return self.bad_gateway(&refusal.text);
                        }
                    }
                }
                let Some((host, target)) = split_url(&url) else {
                    return self.bad_gateway("unreadable URL");
                };
                if !self.file.serves(&host) {
                    return self.unanswerable(&host);
                }
                let injected = match &header {
                    None => format!("Bearer {}", self.credential),
                    Some(h) => format!("{}{}", h.prefix, self.credential),
                };
                let answer = upstream(
                    &self.file,
                    &host,
                    &grant.method,
                    &target,
                    &headers,
                    header.as_ref().map_or("authorization", |h| h.name.as_str()),
                    &injected,
                );
                let placeholder = self
                    .envelope
                    .binding(&grant.connector, &grant.binding)
                    .map_or("", |b| b.placeholder.as_str());
                // The key, and the whole header value a provider may echo as it is.
                let redactor = SecretRedactor::with_replacement(
                    &[injected.as_bytes(), self.credential.as_bytes()],
                    placeholder,
                );
                let body = redactor.redact_bytes(answer.body.as_bytes()).into_owned();
                self.log(Event::Answered(answer.status));
                Reply {
                    status: answer.status,
                    content_type: "application/json",
                    body,
                }
            }
            Ok(None) if may_carry_placeholder(&request) => {
                let text = "Connection credential refused: the request carries a placeholder that no binding of this connector declares.".to_string();
                self.log(Event::Refused(text.clone()));
                self.bad_gateway(&text)
            }
            Ok(None) => {
                // No credential is attached: the mock wants one, so it says 401.
                let Some((host, _)) = split_url(&url) else {
                    return self.bad_gateway("unreadable URL");
                };
                if !self.file.serves(&host) {
                    return self.unanswerable(&host);
                }
                self.log(Event::Answered(401));
                Reply {
                    status: 401,
                    content_type: "application/json",
                    body: br#"{"error":"unauthorized"}"#.to_vec(),
                }
            }
        }
    }

    /// A host the test file does not declare: nothing leaves this machine, so nothing answers.
    fn unanswerable(&self, host: &str) -> Reply {
        let why = format!(
            "`{host}` is not one of the test file's hosts ({}), and `vam connector test` sends nothing off this machine",
            self.file.hosts.join(", ")
        );
        self.log(Event::Unanswered(why.clone()));
        self.bad_gateway(&why)
    }
}

/// The declared mock: every route needs the credential where the binding puts it, `Authorization:
/// Bearer <credential>` or the named header with its prefix (`expected`; 401 otherwise); a request
/// no route matches is 404 (behind the key), as the e2e runner's declared mock answers.
fn upstream(
    file: &ConnectorTestFile,
    host: &str,
    method: &str,
    target: &str,
    headers: &HashMap<String, String>,
    header_name: &str,
    expected: &str,
) -> MockAnswer {
    let keyed = headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case(header_name) && v == expected);
    if keyed {
        file.answer_for(host, method, target)
    } else {
        MockAnswer {
            status: 401,
            body: r#"{"error":"unauthorized"}"#.into(),
        }
    }
}

/// The host (lowercase, no port) and the target (path and query) of an http(s) URL.
fn split_url(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("HTTPS://"))
        .or_else(|| url.strip_prefix("HTTP://"))?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
    if host.is_empty() {
        return None;
    }
    let target = if tail.is_empty() { "/" } else { tail };
    Some((host.to_ascii_lowercase(), target.to_string()))
}

struct Server {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(stub: Arc<Stub>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let stub = stub.clone();
                        std::thread::spawn(move || serve(stream, &stub));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            port,
            stop,
            thread: Some(thread),
        })
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(mut stream: TcpStream, stub: &Stub) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Some(req) = read_request(&mut stream) else {
        return;
    };
    let reply = stub.handle(&req);
    let head_only = req.method.eq_ignore_ascii_case("HEAD");
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        reason(reply.status),
        reply.content_type,
        reply.body.len()
    )
    .into_bytes();
    if !head_only {
        out.extend_from_slice(&reply.body);
    }
    let _ = stream.write_all(&out);
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

fn read_request(stream: &mut TcpStream) -> Option<RawRequest> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if buf.len() > MAX_REQUEST {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = std::str::from_utf8(&buf[..head_end]).ok()?.to_string();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    let length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    if length > MAX_REQUEST {
        return None;
    }
    let body_start = head_end + 4;
    while buf.len() < body_start + length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    Some(RawRequest {
        method,
        target,
        headers,
        body: buf[body_start..body_start + length].to_vec(),
    })
}

#[cfg(test)]
mod tests;
