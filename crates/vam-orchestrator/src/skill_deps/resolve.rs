//! `Resolver` (VI-1.5): plan a skill's lockfiles, serve each from the cache or fetch its packages
//! from the operator's mirror, hash-check, unpack safely, and publish the tree atomically.

use super::cache::{self, Cache, DepsJson, Entry, PkgRecord, Published};
use super::files::SkillFiles;
use super::index::{
    npm_tarball_locator, parse_index, python_file_locator, python_index_locator, select_wheel,
};
use super::limits::Limits;
use super::mirrors::{MirrorSpec, Mirrors};
use super::npm::{HashAlg, Integrity, NpmPkg};
use super::plan::{hex, plan_full, LockPackages, LockPlan};
use super::requirements::PyReq;
use super::target::Target;
use super::transport::{Fetched, FileTransport, HttpTransport, Locator, Transport};
use super::unpack::{fmt_cap, unpack_tgz, unpack_wheel, Ledger, Pkg};
use super::{refused, unavailable, DepsError, Ecosystem, Planned, Source, FETCHER_VERSION};
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A resolved ecosystem tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub ecosystem: Ecosystem,
    pub key: String,
    pub source: Source,
    /// `<drive9>/deps/<key>`.
    pub dir: PathBuf,
    /// `dir/python` or `dir/node_modules`: what VI-1.7 links into the staged skill.
    pub tree: PathBuf,
    pub package_count: u32,
    pub file_count: u32,
    pub bytes: u64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------------------------
// Process-wide gate: a member uploading many skills cannot saturate disk or egress.
// ---------------------------------------------------------------------------------------------

pub(crate) struct Gate {
    slots: Mutex<usize>,
    cv: Condvar,
}

pub(crate) struct Permit<'a>(&'a Gate);

impl Gate {
    pub(crate) fn new(n: usize) -> Gate {
        Gate {
            slots: Mutex::new(n),
            cv: Condvar::new(),
        }
    }

    pub(crate) fn acquire(&self, deadline: Instant) -> Result<Permit<'_>, DepsError> {
        let mut slots = lock(&self.slots);
        while *slots == 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(unavailable(
                    "the node is busy resolving other skills' packages; try again shortly",
                ));
            }
            let (guard, _) = self
                .cv
                .wait_timeout(slots, left)
                .unwrap_or_else(|e| e.into_inner());
            slots = guard;
        }
        *slots -= 1;
        Ok(Permit(self))
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *lock(&self.0.slots) += 1;
        self.0.cv.notify_one();
    }
}

static GATE: LazyLock<Gate> = LazyLock::new(|| Gate::new(2));

// ---------------------------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------------------------

/// Plans, fetches and caches skill packages. Cheap to share: callers keep one per node.
pub struct Resolver {
    mirrors: Mirrors,
    cache: Cache,
    limits: Limits,
    in_flight: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Set only by the test module (`with_transport`); always `None` in a running node.
    transport_override: Option<Arc<dyn Transport>>,
}

impl Resolver {
    /// `drive9_root` is where `deps/` lives. A failure to create the directories shows up on the
    /// first call that needs them.
    pub fn new(mirrors: Mirrors, drive9_root: &Path, limits: Limits) -> Resolver {
        let cache = Cache::new(drive9_root);
        let _ = cache.ensure();
        Resolver {
            mirrors,
            cache,
            limits,
            in_flight: Mutex::new(HashMap::new()),
            transport_override: None,
        }
    }

    /// VI-1.6 upload and marketplace import: plan, use the cache or fetch. Blocking, up to the
    /// deadline. Call it from `spawn_blocking`.
    pub fn resolve(&self, files: &SkillFiles, target: &Target) -> Result<Vec<Resolved>, DepsError> {
        let plans = plan_full(files, target, &self.limits)?;
        if plans.is_empty() {
            return Ok(Vec::new());
        }
        let deadline = Instant::now() + self.limits.resolve_deadline;
        let _permit = GATE.acquire(deadline)?;
        let mut out = Vec::with_capacity(plans.len());
        for p in plans {
            out.push(self.resolve_one(p, target, deadline)?);
        }
        self.check_skill_caps(&out)?;
        Ok(out)
    }

    /// VI-1.6 release and VI-1.7 run start: the cache only, never the network. `None` is a miss.
    pub fn lookup(
        &self,
        files: &SkillFiles,
        target: &Target,
    ) -> Result<Vec<(Planned, Option<Resolved>)>, DepsError> {
        let plans = plan_full(files, target, &self.limits)?;
        let mut out = Vec::with_capacity(plans.len());
        for p in plans {
            let hit = self
                .cache
                .lookup(&p.planned.key, p.planned.ecosystem, &self.limits)?
                .map(|e| to_resolved(&p.planned, e, Source::Cache));
            out.push((p.planned, hit));
        }
        let hits: Vec<Resolved> = out.iter().filter_map(|(_, r)| r.clone()).collect();
        self.check_skill_caps(&hits)?;
        Ok(out)
    }

    /// Remove staging and trash left by a crash; `vamd` calls it once at startup.
    pub fn sweep_stale_tmp(&self, older_than: Duration) -> usize {
        self.cache.sweep_stale_tmp(older_than)
    }

    fn check_skill_caps(&self, resolved: &[Resolved]) -> Result<(), DepsError> {
        let bytes: u64 = resolved.iter().map(|r| r.bytes).sum();
        let files: u64 = resolved.iter().map(|r| r.file_count as u64).sum();
        if bytes > self.limits.max_unpacked_skill_bytes {
            return Err(refused(format!(
                "skill packages are over the {} limit (unpacked)",
                fmt_cap(self.limits.max_unpacked_skill_bytes)
            )));
        }
        if files > self.limits.max_skill_files as u64 {
            return Err(refused(format!(
                "skill packages are over the {} file limit",
                self.limits.max_skill_files
            )));
        }
        Ok(())
    }

    fn transport(&self, eco: Ecosystem) -> Arc<dyn Transport> {
        if let Some(t) = &self.transport_override {
            return t.clone();
        }
        match self.mirrors.for_ecosystem(eco) {
            MirrorSpec::Https { hosts, .. } => {
                Arc::new(HttpTransport::new(hosts.clone(), &self.limits))
            }
            MirrorSpec::File { root } => Arc::new(FileTransport::new(root.clone())),
        }
    }

    fn resolve_one(
        &self,
        plan: LockPlan,
        target: &Target,
        deadline: Instant,
    ) -> Result<Resolved, DepsError> {
        let planned = &plan.planned;
        if let Some(e) = self
            .cache
            .lookup(&planned.key, planned.ecosystem, &self.limits)?
        {
            return Ok(to_resolved(planned, e, Source::Cache));
        }
        // Single flight per key: the second caller waits, then finds the finished tree.
        let key_lock = {
            let mut map = lock(&self.in_flight);
            map.entry(planned.key.clone()).or_default().clone()
        };
        let result = {
            let _held = lock(&key_lock);
            match self
                .cache
                .lookup(&planned.key, planned.ecosystem, &self.limits)?
            {
                Some(e) => Ok(to_resolved(planned, e, Source::Cache)),
                None => self.fetch_and_publish(&plan, target, deadline),
            }
        };
        {
            let mut map = lock(&self.in_flight);
            // The map and `key_lock` hold the only references once every waiter is done.
            if Arc::strong_count(&key_lock) <= 2 {
                map.remove(&planned.key);
            }
        }
        result
    }

    fn fetch_and_publish(
        &self,
        plan: &LockPlan,
        target: &Target,
        deadline: Instant,
    ) -> Result<Resolved, DepsError> {
        let planned = &plan.planned;
        let staging = self.cache.new_staging(&planned.key)?;
        let transport = self.transport(planned.ecosystem);
        let mut ledger = Ledger::default();
        let records = match &plan.packages {
            LockPackages::Python(reqs) => self.build_python(
                reqs,
                target,
                &*transport,
                &staging.out(),
                &staging.downloads(),
                &mut ledger,
                deadline,
            )?,
            LockPackages::Js(pkgs) => self.build_js(
                pkgs,
                &*transport,
                &staging.out(),
                &staging.downloads(),
                &mut ledger,
                deadline,
            )?,
        };
        // The tree directory exists even when every package was empty.
        std::fs::create_dir_all(staging.out().join(cache::tree_name(planned.ecosystem)))?;
        let json = DepsJson {
            v: 1,
            ecosystem: planned.ecosystem.as_str().to_string(),
            key: planned.key.clone(),
            target: target.abi_for(planned.ecosystem),
            fetcher: FETCHER_VERSION,
            files: ledger.files as u64,
            bytes: ledger.bytes,
            packages: records,
        };
        let body = serde_json::to_vec_pretty(&json)
            .map_err(|e| DepsError::Io(std::io::Error::other(e)))?;
        std::fs::write(staging.out().join("deps.json"), body)?;
        // Downloads are not part of the tree.
        let _ = std::fs::remove_dir_all(staging.downloads());

        for attempt in 0..2 {
            match self.cache.publish(&staging, &planned.key)? {
                Published::Won => {
                    let entry = self
                        .cache
                        .lookup(&planned.key, planned.ecosystem, &self.limits)?
                        .ok_or_else(|| {
                            DepsError::Io(std::io::Error::other("published tree failed validation"))
                        })?;
                    return Ok(to_resolved(planned, entry, Source::Mirror));
                }
                Published::Lost => {
                    // Another writer's rename landed first. Use its tree if it is sound.
                    if let Some(entry) =
                        self.cache
                            .lookup(&planned.key, planned.ecosystem, &self.limits)?
                    {
                        return Ok(to_resolved(planned, entry, Source::Mirror));
                    }
                    if attempt == 1 {
                        break;
                    }
                    // The winner's tree was bad and has been trashed; rename ours in.
                }
            }
        }
        Err(DepsError::Io(std::io::Error::other(
            "could not publish the package tree",
        )))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_python(
        &self,
        reqs: &[PyReq],
        target: &Target,
        transport: &dyn Transport,
        out: &Path,
        dl: &Path,
        ledger: &mut Ledger,
        deadline: Instant,
    ) -> Result<Vec<PkgRecord>, DepsError> {
        let spec = self.mirrors.for_ecosystem(Ecosystem::Python);
        let mut records = Vec::new();
        for (i, req) in reqs.iter().enumerate() {
            let label = format!("package '{}' {}", req.name, req.version);
            let index_loc = python_index_locator(spec, &req.name)?;
            let mut body = Vec::new();
            match transport
                .get(
                    &index_loc,
                    true,
                    self.limits.max_index_bytes,
                    deadline,
                    &mut body,
                )
                .map_err(|e| with_package(e, &label))?
            {
                Fetched::Body(_) => {}
                Fetched::NotFound => {
                    let first = req
                        .hashes
                        .first()
                        .map(|h| &h[..12.min(h.len())])
                        .unwrap_or("");
                    return Err(refused(format!(
                        "{label} is not in the package mirror (sha256 {first}...)"
                    )));
                }
            }
            let files = parse_index(&body, &req.name)?;
            let chosen = select_wheel(req, &files, target)?;
            let file_loc = python_file_locator(spec, &index_loc, &chosen)?;
            let expected_hex = chosen.sha256.clone().unwrap_or_default();
            let expected = Integrity {
                alg: HashAlg::Sha256,
                digest: from_hex(&expected_hex)
                    .ok_or_else(|| refused(format!("{label}: the mirror's hash is malformed")))?,
            };
            let archive = dl.join(format!("{i}.whl"));
            let bytes =
                self.download(transport, &file_loc, &expected, &archive, &label, deadline)?;
            let pkg = Pkg {
                name: &req.name,
                version: &req.version,
            };
            unpack_wheel(&archive, out, "python", &pkg, ledger, &self.limits)?;
            let _ = std::fs::remove_file(&archive);
            records.push(PkgRecord {
                name: req.name.clone(),
                version: req.version.clone(),
                file: chosen.filename.clone(),
                sha256: Some(expected_hex),
                integrity: None,
                bytes,
            });
        }
        Ok(records)
    }

    fn build_js(
        &self,
        pkgs: &[NpmPkg],
        transport: &dyn Transport,
        out: &Path,
        dl: &Path,
        ledger: &mut Ledger,
        deadline: Instant,
    ) -> Result<Vec<PkgRecord>, DepsError> {
        let spec = self.mirrors.for_ecosystem(Ecosystem::Js);
        let mut records = Vec::new();
        for (i, p) in pkgs.iter().enumerate() {
            let label = format!("package '{}' {}", p.name, p.version);
            let loc = npm_tarball_locator(spec, &p.name, &p.version)?;
            let archive = dl.join(format!("{i}.tgz"));
            let bytes = self.download(transport, &loc, &p.integrity, &archive, &label, deadline)?;
            let pkg = Pkg {
                name: &p.name,
                version: &p.version,
            };
            unpack_tgz(&archive, out, &p.path, &pkg, ledger, &self.limits)?;
            let _ = std::fs::remove_file(&archive);
            let file = match &loc {
                Locator::Http(u) => u
                    .path_segments()
                    .and_then(|mut s| s.next_back())
                    .unwrap_or("")
                    .to_string(),
                Locator::File(f) => f
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            };
            records.push(PkgRecord {
                name: p.name.clone(),
                version: p.version.clone(),
                file,
                sha256: None,
                integrity: Some(format!(
                    "{}-{}",
                    p.integrity.alg.as_str(),
                    hex(&p.integrity.digest)
                )),
                bytes,
            });
        }
        Ok(records)
    }

    /// Stream one file into `dest` while hashing it; the hash is compared before anything is
    /// unpacked, and a mismatch deletes the file.
    fn download(
        &self,
        transport: &dyn Transport,
        loc: &Locator,
        expected: &Integrity,
        dest: &Path,
        label: &str,
        deadline: Instant,
    ) -> Result<u64, DepsError> {
        let file = std::fs::File::create(dest)?;
        let mut sink = HashingWriter {
            file,
            hasher: Hasher::new(expected.alg),
        };
        let fetched = transport
            .get(
                loc,
                false,
                self.limits.max_download_bytes,
                deadline,
                &mut sink,
            )
            .map_err(|e| {
                let _ = std::fs::remove_file(dest);
                match e {
                    DepsError::Io(io) if io.raw_os_error() == Some(libc::ENOSPC) => {
                        unavailable("the node's disk is full; free space and try again")
                    }
                    other => with_package(other, label),
                }
            })?;
        let n = match fetched {
            Fetched::Body(n) => n,
            Fetched::NotFound => {
                let _ = std::fs::remove_file(dest);
                return Err(refused(format!("{label} is not in the package mirror")));
            }
        };
        let HashingWriter { file, hasher } = sink;
        drop(file);
        if hasher.finalize() != expected.digest {
            let _ = std::fs::remove_file(dest);
            return Err(refused(format!(
                "{label}: the downloaded file does not match its locked hash"
            )));
        }
        Ok(n)
    }
}

fn to_resolved(planned: &Planned, e: Entry, source: Source) -> Resolved {
    Resolved {
        ecosystem: planned.ecosystem,
        key: planned.key.clone(),
        source,
        dir: e.dir,
        tree: e.tree,
        package_count: e.package_count,
        file_count: e.file_count,
        bytes: e.bytes,
    }
}

/// Name the package in an error from the transport.
fn with_package(e: DepsError, label: &str) -> DepsError {
    const PREFIX: &str = "skill dependencies: ";
    let rebuild = |m: String| {
        let rest = m.strip_prefix(PREFIX).unwrap_or(&m);
        format!("{PREFIX}{label}: {rest}")
    };
    match e {
        DepsError::Refused(m) => DepsError::Refused(rebuild(m)),
        DepsError::Unavailable(m) => DepsError::Unavailable(rebuild(m)),
        other => other,
    }
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

enum Hasher {
    S256(Sha256),
    S384(Sha384),
    S512(Sha512),
}

impl Hasher {
    fn new(alg: HashAlg) -> Hasher {
        match alg {
            HashAlg::Sha256 => Hasher::S256(Sha256::new()),
            HashAlg::Sha384 => Hasher::S384(Sha384::new()),
            HashAlg::Sha512 => Hasher::S512(Sha512::new()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::S256(h) => h.update(data),
            Hasher::S384(h) => h.update(data),
            Hasher::S512(h) => h.update(data),
        }
    }

    fn finalize(self) -> Vec<u8> {
        match self {
            Hasher::S256(h) => h.finalize().to_vec(),
            Hasher::S384(h) => h.finalize().to_vec(),
            Hasher::S512(h) => h.finalize().to_vec(),
        }
    }
}

struct HashingWriter {
    file: std::fs::File,
    hasher: Hasher,
}

impl Write for HashingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests;
