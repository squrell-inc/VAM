//! Trusted marketplaces on the node (CN-4.4).
//!
//! A marketplace is a Git repository whose root `marketplace.json` lists *listings*: bundles of
//! skills, playbooks and agents released at a tag. An org admin trusts a marketplace by URL; the
//! node reads it (index, tags, and what each listing installs and can reach) and shows it. This
//! module only **reads**: nothing from a marketplace reaches the catalog here (M1). Installing a
//! listing at its pinned tag is CN-4.5 and builds on the cache and pins kept here.
//!
//! - [`url`]: the URL rules and the address classifier (https only, public addresses only).
//! - [`guard`]: the loopback CONNECT proxy that makes those rules hold at connect time.
//! - [`fetch`]: the hardened `git` calls and the bare cache under `<state dir>/marketplaces/<id>`.
//! - [`store`]: the `node_marketplaces` and `node_marketplace_tags` tables.
//! - [`summary`]: the capability summary of a listing.
//!
//! **For CN-4.5.** The cache of marketplace `id` is a bare repository at
//! `Fetcher::cache_dir(id)`; every tag a listing names has been fetched (depth 1) into a ref of
//! its own, and `store.marketplace_pin(id, tag)` is the commit that tag is pinned to. To install
//! "the cached commit of tag X": [`cached_tag_commit`] re-checks the tag against the remote and
//! the pin (a moved tag is `Moved`, never followed) and returns the commit; then
//! `Fetcher::export_dir(id, commit, <artifact path>, <empty dir>)` writes one artifact directory of
//! that exact commit (regular files only) for `vam_spec::package::check_dir`. Each listing's
//! artifacts, the commit and the capability summary of the last read are in
//! [`store::ListingState`].

pub mod connector_store;
pub mod fetch;
pub mod guard;
pub mod import;
pub mod installs;
pub mod store;
pub mod summary;
#[cfg(test)]
pub(crate) mod testutil;
pub mod url;

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use vam_spec::marketplace::{parse_index, NodeRequirement, INDEX_FILE, MAX_INDEX_BYTES};
use vam_spec::package::check_dir;
use vam_spec::RegistryDomains;

use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;
use fetch::{FetchError, Fetcher, Limits, Remote, HEAD_REF};
use store::{ArtifactState, ListingState, MarketplaceRow, RefreshData, TagSeen};

/// The id of the preconfigured Squrell marketplace.
pub const SQURELL_ID: &str = "squrell";
/// Its URL. Only this entry is `trust: squrell`; no added marketplace can be.
pub const SQURELL_URL: &str = "https://github.com/squrell-inc/vam-marketplace";
pub const SQURELL_NAME: &str = "Squrell";
/// The domain Squrell's artifacts live under (`VAMD_MARKETPLACE_DOMAIN` can override it on a
/// staging node). Always reserved: no added marketplace may declare it.
pub const SQURELL_DOMAIN: &str = vam_spec::DEFAULT_MARKETPLACE_DOMAIN;
/// Most marketplaces one node trusts.
pub const MAX_MARKETPLACES: usize = 20;
/// Tags kept in the "seen" list of a marketplace.
const MAX_TAGS_SEEN: usize = 500;
/// A whole read of one marketplace may take this long.
const READ_BUDGET: Duration = Duration::from_secs(5 * 60);
const DEFAULT_REFRESH_SECS: u64 = 6 * 60 * 60;
const MIN_REFRESH_SECS: u64 = 5 * 60;

pub const ENV_GIT_BIN: &str = "VAMD_GIT_BIN";
pub const ENV_REFRESH_SECS: &str = "VAMD_MARKETPLACE_REFRESH_SECS";

// ---- configuration ----------------------------------------------------------------------------

/// What marketplace reads run with; installed once by `vamd`.
#[derive(Debug, Clone)]
pub struct MarketplaceConfig {
    pub fetcher: Fetcher,
    /// How often an enabled marketplace is read on its own. `None`: only on demand.
    pub refresh_every: Option<Duration>,
    pub node_version: String,
}

impl MarketplaceConfig {
    /// From the environment: `VAMD_GIT_BIN` (else `git` on `PATH`) and
    /// `VAMD_MARKETPLACE_REFRESH_SECS` (default 21600; `0` or `off` for never; at least 300).
    pub fn from_env(state_dir: &Path) -> Self {
        let refresh_every = match std::env::var(ENV_REFRESH_SECS)
            .ok()
            .as_deref()
            .map(str::trim)
        {
            None | Some("") => Some(DEFAULT_REFRESH_SECS),
            Some("0") | Some("off") => None,
            Some(v) => Some(
                v.parse::<u64>()
                    .map(|s| s.max(MIN_REFRESH_SECS))
                    .unwrap_or(DEFAULT_REFRESH_SECS),
            ),
        }
        .map(Duration::from_secs);
        Self {
            fetcher: Fetcher::new(
                resolve_git_bin(std::env::var(ENV_GIT_BIN).ok().as_deref()),
                state_dir.join("marketplaces"),
                Limits::default(),
            ),
            refresh_every,
            node_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// The `git` to run: `VAMD_GIT_BIN` when it names an executable file, else the first `git` on
/// `PATH`. Always an absolute path (the child runs with a cleared environment).
pub fn resolve_git_bin(configured: Option<&str>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let usable = |p: &Path| {
        p.is_absolute()
            && std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    };
    if let Some(c) = configured.map(str::trim).filter(|c| !c.is_empty()) {
        let p = PathBuf::from(c);
        return usable(&p).then_some(p);
    }
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|d| Path::new(d).join("git"))
        .find(|p| usable(p))
}

static CONFIG: OnceLock<MarketplaceConfig> = OnceLock::new();

/// Installs the configuration (once, at boot).
pub fn install(cfg: MarketplaceConfig) {
    let _ = CONFIG.set(cfg);
}

pub fn config() -> Result<&'static MarketplaceConfig, OrchestratorError> {
    CONFIG.get().ok_or_else(|| {
        OrchestratorError::InvalidContract("marketplaces are not set up on this node".into())
    })
}

/// A lowercase domain name `vam_spec` accepts as a registry domain.
pub(crate) fn is_domain_name(d: &str) -> bool {
    !d.trim().is_empty() && RegistryDomains::new(d, None).is_ok()
}

// ---- domain rules (M3) -------------------------------------------------------------------------

/// Why an added marketplace may not declare `domain` as its publisher domain, or `None`. An added
/// marketplace cannot write under the Squrell domain, the org's registry domain, `local.vam.dev`,
/// `*.local`, `examples.local`, or another trusted marketplace's domain.
pub fn reserved_domain_reason(
    domain: &str,
    domains: &RegistryDomains,
    others: &[&MarketplaceRow],
) -> Option<String> {
    let squrell_sub = format!(".{SQURELL_DOMAIN}");
    if domain == SQURELL_DOMAIN
        || domain.ends_with(&squrell_sub)
        || (!domains.marketplace_domain().is_empty() && domain == domains.marketplace_domain())
    {
        return Some(format!(
            "`{domain}` is Squrell's domain; only the preconfigured Squrell marketplace may publish under it"
        ));
    }
    if domain == domains.registry_domain() {
        return Some(format!(
            "`{domain}` is this org's own registry domain; a marketplace may not publish under it"
        ));
    }
    if vam_spec::is_local_skill_registry(domain) {
        return Some(format!(
            "`{domain}` is a local catalog registry (local.vam.dev, *.local, examples.local); a marketplace may not publish under it"
        ));
    }
    others
        .iter()
        .find(|m| m.publisher_domain == domain)
        .map(|m| {
            format!(
                "`{domain}` is already the publisher domain of the trusted marketplace `{}`",
                m.name
            )
        })
}

fn imitates_squrell(name: &str) -> bool {
    let norm: String = name
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    norm == "squrell"
}

// ---- reading a marketplace ---------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("{0}")]
    Fetch(#[from] FetchError),
    #[error("the marketplace index is not usable: {0}")]
    Index(String),
    #[error("the index declares publisherDomain `{declared}`, not the confirmed `{confirmed}`")]
    DomainChanged { declared: String, confirmed: String },
}

/// A tag that now names another commit than the one it was pinned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedTag {
    pub tag: String,
    pub pinned: String,
    pub now: String,
}

impl MovedTag {
    pub fn message(&self) -> String {
        format!(
            "tag `{}` moved from {} to {}; a moved tag is refused, never followed",
            self.tag,
            &self.pinned[..self.pinned.len().min(10)],
            &self.now[..self.now.len().min(10)]
        )
    }
}

pub struct ReadInput<'a> {
    pub id: &'a str,
    pub remote: Remote,
    /// The publisher domain an admin confirmed; the index must declare exactly this.
    pub confirmed_domain: &'a str,
    pub pins: &'a BTreeMap<String, String>,
    pub node_version: &'a str,
}

pub struct ReadResult {
    pub data: RefreshData,
    pub moved: Vec<MovedTag>,
}

/// One read of a marketplace: its advertised refs, the index at the default branch head, and for
/// every listing the tag's commit (pinned the first time), the artifacts exported from that exact
/// commit, each checked with the shared package check, and the capability summary. Reads only.
pub fn read_marketplace(f: &Fetcher, input: &ReadInput<'_>) -> Result<ReadResult, ReadError> {
    let started = Instant::now();
    let id = input.id;
    f.precheck(&input.remote)?;
    let refs = f.ls_remote(&input.remote)?;
    // A cache that grew past its cap is dropped and rebuilt; installed commits are fetched again
    // from their pinned tags when needed.
    if let Ok(dir) = f.cache_dir(id) {
        if fetch::dir_size(&dir) > f.limits.max_cache_bytes {
            f.wipe(id)?;
        }
    }
    f.fetch(id, &input.remote, &[])?;
    let head = f
        .ref_commit(id, HEAD_REF)?
        .ok_or_else(|| FetchError::Invalid("the default branch was not fetched".into()))?;
    let raw = f
        .read_file(id, &head, INDEX_FILE, MAX_INDEX_BYTES as u64)
        .map_err(|e| ReadError::Index(e.to_string()))?;
    let digest = {
        use sha2::{Digest, Sha256};
        Sha256::digest(&raw)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let text = String::from_utf8(raw)
        .map_err(|_| ReadError::Index(format!("{INDEX_FILE} is not UTF-8 text")))?;
    let index = parse_index(&text).map_err(|problems| {
        ReadError::Index(
            problems
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        )
    })?;
    if index.publisher_domain != input.confirmed_domain {
        return Err(ReadError::DomainChanged {
            declared: index.publisher_domain,
            confirmed: input.confirmed_domain.to_string(),
        });
    }

    // Which tags to read: those the listings name that exist and have not moved.
    let mut listings: Vec<ListingState> = Vec::new();
    let mut moved: Vec<MovedTag> = Vec::new();
    let mut to_fetch: Vec<String> = Vec::new();
    for l in &index.listings {
        let mut state = ListingState {
            id: l.id.clone(),
            display_name: l.display_name.clone(),
            description: l.description.clone(),
            connector: l.connector.clone(),
            git_ref: l.git_ref.clone(),
            requires_node: l.requires_node.clone(),
            commit: None,
            artifacts: Vec::new(),
            summary: None,
            error: None,
        };
        match refs.tags.get(&l.git_ref) {
            None => {
                state.error = Some(format!(
                    "tag `{}` does not exist in the repository",
                    l.git_ref
                ))
            }
            Some(now) => match input.pins.get(&l.git_ref) {
                Some(pinned) if pinned != now => {
                    let m = MovedTag {
                        tag: l.git_ref.clone(),
                        pinned: pinned.clone(),
                        now: now.clone(),
                    };
                    state.commit = Some(pinned.clone());
                    state.error = Some(m.message());
                    if !moved.iter().any(|x| x.tag == m.tag) {
                        moved.push(m);
                    }
                }
                _ => {
                    if !to_fetch.contains(&l.git_ref) {
                        to_fetch.push(l.git_ref.clone());
                    }
                }
            },
        }
        listings.push(state);
    }
    if !to_fetch.is_empty() {
        f.fetch(id, &input.remote, &to_fetch)?;
    }

    // Each tag is read from the cache at the commit it names, which must be the commit the remote
    // advertised (a tag that changes while it is read is not trusted).
    let mut new_pins: Vec<(String, String)> = Vec::new();
    let mut fetched: BTreeMap<String, String> = BTreeMap::new();
    for tag in &to_fetch {
        let want = &refs.tags[tag];
        match f.tag_commit(id, tag)? {
            Some(c) if &c == want => {
                fetched.insert(tag.clone(), c.clone());
                if !input.pins.contains_key(tag) {
                    new_pins.push((tag.clone(), c));
                }
            }
            _ => {}
        }
    }
    let node_ok = |req: &str| match NodeRequirement::parse(req) {
        Ok(r) => {
            if r.matches(input.node_version) {
                Ok(())
            } else {
                Err(format!(
                    "requires node {req}; this node is {}",
                    input.node_version
                ))
            }
        }
        Err(why) => Err(format!("requiresNode {why}")),
    };
    for (state, l) in listings.iter_mut().zip(&index.listings) {
        if state.error.is_some() {
            continue;
        }
        if started.elapsed() > READ_BUDGET {
            return Err(FetchError::Timeout("read of the marketplace").into());
        }
        let Some(commit) = fetched.get(&l.git_ref) else {
            state.error = Some(format!(
                "tag `{}` changed while it was being read; try again",
                l.git_ref
            ));
            continue;
        };
        state.commit = Some(commit.clone());
        if let Err(why) = node_ok(&l.requires_node) {
            state.error = Some(why);
            continue;
        }
        match summarize_listing(f, id, commit, l, &index.publisher_domain) {
            Ok((artifacts, summary)) => {
                state.artifacts = artifacts;
                state.summary = Some(summary);
            }
            Err(why) => state.error = Some(why),
        }
    }
    let tags_seen = refs
        .tags
        .iter()
        .take(MAX_TAGS_SEEN)
        .map(|(tag, commit)| TagSeen {
            tag: tag.clone(),
            commit: commit.clone(),
        })
        .collect();
    Ok(ReadResult {
        data: RefreshData {
            name: index.name,
            index_digest: digest,
            head_commit: head,
            listings,
            tags_seen,
            new_pins,
        },
        moved,
    })
}

/// Exports each artifact of a listing from `commit` into a scratch directory, runs the shared
/// package check on it, and returns the artifacts and the listing's capability summary. Any
/// failure is the listing's error.
fn summarize_listing(
    f: &Fetcher,
    id: &str,
    commit: &str,
    l: &vam_spec::marketplace::Listing,
    publisher_domain: &str,
) -> Result<(Vec<ArtifactState>, Value), String> {
    let mut artifacts = Vec::new();
    let mut summaries = Vec::new();
    for a in &l.artifacts {
        let fail = |why: String| format!("{} `{}`: {}", a.kind.as_str(), a.path, f.scrub(&why));
        let scratch = f.scratch(id).map_err(|e| fail(e.to_string()))?;
        f.export_dir(id, commit, &a.path, scratch.path())
            .map_err(|e| fail(e.to_string()))?;
        let checked = check_dir(scratch.path(), Some(a.kind)).map_err(|e| fail(e.0.clone()))?;
        if vam_spec::marketplace::urn_domain(&checked.urn) != Some(publisher_domain) {
            return Err(fail(format!(
                "the artifact URN {} is outside the marketplace's publisher domain {publisher_domain}",
                checked.urn
            )));
        }
        artifacts.push(ArtifactState {
            kind: a.kind.as_str().to_string(),
            path: a.path.clone(),
            urn: checked.urn.clone(),
        });
        summaries.push(checked.summary);
    }
    Ok((
        artifacts,
        summary::listing_summary(&summaries, l.connector.as_deref()),
    ))
}

/// For CN-4.5: the commit of `tag` to install, re-checked now. The tag is looked up on the remote
/// again; it must still name the commit it is pinned to (`Moved` otherwise, never followed), and
/// that commit is fetched into the cache if it is not there. Returns the pinned commit.
pub fn cached_tag_commit(
    store: &SqliteWorkflowStore,
    cfg: &MarketplaceConfig,
    row: &MarketplaceRow,
    tag: &str,
) -> Result<String, CachedTagError> {
    let pinned = store
        .marketplace_pin(&row.id, tag)
        .map_err(|e| CachedTagError::Other(e.to_string()))?
        .ok_or_else(|| CachedTagError::Unpinned(tag.to_string()))?;
    let u = url::check_url(&row.url).map_err(|e| CachedTagError::Other(e.to_string()))?;
    let remote = Remote::for_url(&u);
    let f = &cfg.fetcher;
    f.precheck(&remote)?;
    let refs = f.ls_remote(&remote)?;
    match refs.tags.get(tag) {
        None => return Err(CachedTagError::Gone(tag.to_string())),
        Some(now) if *now != pinned => {
            return Err(CachedTagError::Moved(MovedTag {
                tag: tag.to_string(),
                pinned,
                now: now.clone(),
            }))
        }
        Some(_) => {}
    }
    if f.tag_commit(&row.id, tag)?.as_deref() != Some(pinned.as_str()) {
        f.fetch(&row.id, &remote, &[tag.to_string()])?;
        if f.tag_commit(&row.id, tag)?.as_deref() != Some(pinned.as_str()) {
            return Err(CachedTagError::Gone(tag.to_string()));
        }
    }
    Ok(pinned)
}

#[derive(Debug, thiserror::Error)]
pub enum CachedTagError {
    #[error("tag `{0}` was never read from this marketplace; refresh it first")]
    Unpinned(String),
    #[error("tag `{0}` no longer exists in the marketplace repository")]
    Gone(String),
    #[error("{}", .0.message())]
    Moved(MovedTag),
    #[error("{0}")]
    Fetch(#[from] FetchError),
    #[error("{0}")]
    Other(String),
}

// ---- operations the API and the timer call ------------------------------------------------------

fn now() -> i64 {
    crate::connectors::now_secs()
}

fn new_id() -> String {
    let n: u64 = rand::random();
    format!("m-{:010x}", n & 0xff_ffff_ffff)
}

fn read_error_text(e: &ReadError) -> String {
    let t = e.to_string();
    t.chars().take(400).collect()
}

/// The outcome of a refresh: the row afterwards, and the refusal text when the read was refused
/// (a moved tag, a changed publisher domain) rather than merely failed.
pub struct Refreshed {
    pub row: MarketplaceRow,
    pub refused: Option<String>,
}

/// Reads one marketplace and records the result. A failed read (network, a broken index, a limit)
/// is recorded on that marketplace as `lastError` and is not an `Err`; the listings of the last
/// good read stay. Blocking: run it on a blocking thread.
pub fn refresh_blocking(
    store: &SqliteWorkflowStore,
    cfg: &MarketplaceConfig,
    id: &str,
) -> Result<Refreshed, OrchestratorError> {
    let row = store
        .marketplace(id)?
        .ok_or_else(|| OrchestratorError::NotFound(format!("marketplace `{id}`")))?;
    if !row.enabled {
        return Err(OrchestratorError::Conflict(format!(
            "marketplace `{}` is disabled; enable it first",
            row.name
        )));
    }
    let t = now();
    let u = match url::check_url(&row.url) {
        Ok(u) => u,
        Err(e) => {
            let msg = e.to_string();
            store.record_marketplace_read(id, t, None, Some(&msg))?;
            return Ok(Refreshed {
                row: reload(store, id)?,
                refused: Some(msg),
            });
        }
    };
    if !row.preconfigured {
        let all = store.marketplaces()?;
        let others: Vec<&MarketplaceRow> = all.iter().filter(|m| m.id != row.id).collect();
        if let Some(why) = reserved_domain_reason(
            &row.publisher_domain,
            &crate::registry::registry_domains(),
            &others,
        ) {
            store.record_marketplace_read(id, t, None, Some(&why))?;
            return Ok(Refreshed {
                row: reload(store, id)?,
                refused: Some(why),
            });
        }
    }
    let pins = store.marketplace_pins(id)?;
    let input = ReadInput {
        id,
        remote: Remote::for_url(&u),
        confirmed_domain: &row.publisher_domain,
        pins: &pins,
        node_version: &cfg.node_version,
    };
    let mut refused = None;
    match read_marketplace(&cfg.fetcher, &input) {
        Ok(read) => {
            let mut data = read.data;
            if row.preconfigured {
                // The preconfigured entry keeps its fixed display name.
                data.name = row.name.clone();
            }
            if !row.preconfigured && imitates_squrell(&data.name) {
                let msg = format!(
                    "the index is named `{}`, which imitates the Squrell marketplace",
                    data.name
                );
                store.record_marketplace_read(id, t, None, Some(&msg))?;
                refused = Some(msg);
            } else if read.moved.is_empty() {
                store.record_marketplace_read(id, t, Some(&data), None)?;
            } else {
                let msg = read
                    .moved
                    .iter()
                    .map(MovedTag::message)
                    .collect::<Vec<_>>()
                    .join("; ");
                store.record_marketplace_read(id, t, Some(&data), Some(&msg))?;
                refused = Some(msg);
            }
        }
        Err(e @ ReadError::DomainChanged { .. }) => {
            let msg = format!(
                "{}; remove the marketplace and add it again if that is intended",
                read_error_text(&e)
            );
            store.record_marketplace_read(id, t, None, Some(&msg))?;
            refused = Some(msg);
        }
        Err(e) => {
            store.record_marketplace_read(id, t, None, Some(&read_error_text(&e)))?;
        }
    }
    Ok(Refreshed {
        row: reload(store, id)?,
        refused,
    })
}

fn reload(store: &SqliteWorkflowStore, id: &str) -> Result<MarketplaceRow, OrchestratorError> {
    store
        .marketplace(id)?
        .ok_or_else(|| OrchestratorError::NotFound(format!("marketplace `{id}`")))
}

static ADD_LOCK: Mutex<()> = Mutex::new(());

/// Trusts a new marketplace: checks the URL and the confirmed publisher domain, reads it once and,
/// only if that read is clean, records it as `unverified`. Blocking.
pub fn add_blocking(
    store: &SqliteWorkflowStore,
    cfg: &MarketplaceConfig,
    raw_url: &str,
    raw_domain: &str,
    added_by: &str,
) -> Result<MarketplaceRow, OrchestratorError> {
    let invalid = |m: String| OrchestratorError::InvalidContract(m);
    let u = url::check_url(raw_url).map_err(|e| invalid(e.to_string()))?;
    let domain = raw_domain.trim();
    if domain.is_empty() {
        return Err(invalid(
            "publisherDomain is required: confirm the domain the marketplace's index declares"
                .into(),
        ));
    }
    if !is_domain_name(domain) {
        return Err(invalid(format!(
            "publisherDomain `{domain}` is not a domain name (lowercase letters, digits, '-' and '.')"
        )));
    }
    let _serial = ADD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let rows = store.marketplaces()?;
    if rows.len() >= MAX_MARKETPLACES {
        return Err(OrchestratorError::Conflict(format!(
            "this node already trusts {MAX_MARKETPLACES} marketplaces"
        )));
    }
    if let Some(dup) = rows
        .iter()
        .find(|m| url::check_url(&m.url).is_ok_and(|x| x.dedup_key == u.dedup_key))
    {
        return Err(OrchestratorError::Conflict(format!(
            "that repository is already trusted as `{}`",
            dup.name
        )));
    }
    let refs: Vec<&MarketplaceRow> = rows.iter().collect();
    if let Some(why) = reserved_domain_reason(domain, &crate::registry::registry_domains(), &refs) {
        return Err(invalid(format!("publisherDomain {why}")));
    }
    if store.catalog_has_domain(domain)? {
        return Err(OrchestratorError::Conflict(format!(
            "publisherDomain `{domain}` already has artifacts in this org's catalog"
        )));
    }
    let id = new_id();
    let pins = BTreeMap::new();
    let input = ReadInput {
        id: &id,
        remote: Remote::for_url(&u),
        confirmed_domain: domain,
        pins: &pins,
        node_version: &cfg.node_version,
    };
    let read = read_marketplace(&cfg.fetcher, &input).map_err(|e| {
        let _ = cfg.fetcher.wipe(&id);
        invalid(format!(
            "could not read the marketplace: {}",
            read_error_text(&e)
        ))
    })?;
    if imitates_squrell(&read.data.name) {
        let _ = cfg.fetcher.wipe(&id);
        return Err(invalid(format!(
            "the index is named `{}`, which imitates the Squrell marketplace",
            read.data.name
        )));
    }
    if let Err(e) = store.insert_marketplace(&id, &u.canonical, domain, added_by, now(), &read.data)
    {
        let _ = cfg.fetcher.wipe(&id);
        return Err(e);
    }
    reload(store, &id)
}

/// Removes an added marketplace (its pins and its cache). **It is refused while anything from it
/// is installed** (409): the installed rows stay read-only and keep their origin, so the
/// marketplace's domain stays reserved and an agent pinned to them keeps resolving. There is no
/// uninstall in this phase; disabling a marketplace hides its listings and keeps its installs.
pub fn delete_blocking(
    store: &SqliteWorkflowStore,
    cfg: &MarketplaceConfig,
    id: &str,
) -> Result<(), OrchestratorError> {
    let row = reload(store, id)?;
    if row.preconfigured {
        return Err(OrchestratorError::Conflict(
            "the preconfigured marketplace can be disabled, not removed".into(),
        ));
    }
    if let Some(installed) = store.marketplace_installs(id)?.first() {
        return Err(OrchestratorError::Conflict(format!(
            "marketplace `{}` cannot be removed while its listing `{}` is installed: installs are \
             read-only and keep their origin; disable the marketplace to hide its listings",
            row.name, installed.listing_id
        )));
    }
    store.delete_marketplace(id)?;
    if let Err(e) = cfg.fetcher.wipe(id) {
        tracing::warn!(
            marketplace = id,
            "could not remove the marketplace cache: {e}"
        );
    }
    Ok(())
}

// ---- the timer ---------------------------------------------------------------------------------

/// Whether a marketplace is due for its own periodic read.
pub fn is_due(row: &MarketplaceRow, now: i64, every: Duration) -> bool {
    row.enabled
        && row
            .last_checked_at
            .is_none_or(|t| now.saturating_sub(t) >= every.as_secs() as i64)
}

/// False on a process-guest node: its scenarios drive every refresh themselves.
pub fn periodic_refresh_enabled() -> bool {
    #[cfg(feature = "process-guest")]
    if crate::process_mode::is_active() {
        return false;
    }
    true
}

/// Reads every marketplace that is due, one after the other, each under its own lock. One
/// marketplace's failure is recorded on it and touches no other.
pub async fn refresh_due(store: Arc<SqliteWorkflowStore>) {
    let Ok(cfg) = config() else { return };
    let Some(every) = cfg.refresh_every else {
        return;
    };
    let Ok(rows) = store.marketplaces() else {
        return;
    };
    for row in rows.into_iter().filter(|r| is_due(r, now(), every)) {
        let _guard = lock_marketplace(&row.id).await;
        let (s, id) = (store.clone(), row.id.clone());
        match tokio::task::spawn_blocking(move || refresh_blocking(&s, cfg, &id)).await {
            Ok(Ok(r)) => {
                if let Some(e) = r.row.last_error {
                    tracing::warn!(marketplace = %row.id, "marketplace read failed: {e}");
                }
            }
            Ok(Err(e)) => tracing::warn!(marketplace = %row.id, "marketplace read: {e}"),
            Err(e) => tracing::warn!(marketplace = %row.id, "marketplace read: {e}"),
        }
    }
}

/// Serialises everything that reads or changes one marketplace.
pub(crate) async fn lock_marketplace(id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    use std::collections::HashMap;
    type Gates = Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>;
    static LOCKS: OnceLock<Gates> = OnceLock::new();
    let gate = {
        let mut locks = LOCKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if locks.len() >= 64 {
            locks.retain(|_, g| g.strong_count() > 0);
        }
        match locks.get(id).and_then(Weak::upgrade) {
            Some(g) => g,
            None => {
                let g = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(id.to_string(), Arc::downgrade(&g));
                g
            }
        }
    };
    gate.lock_owned().await
}

// ---- JSON views ---------------------------------------------------------------------------------

/// One entry of `GET /v1/marketplaces`. Unset optional fields are absent.
pub fn marketplace_view(row: &MarketplaceRow) -> Value {
    let mut v = json!({
        "id": row.id,
        "name": row.name,
        "url": row.url,
        "publisherDomain": row.publisher_domain,
        "trust": row.trust.as_str(),
        "preconfigured": row.preconfigured,
        "enabled": row.enabled,
        "tagsSeen": row.tags_seen,
    });
    if let Some(t) = row.last_checked_at {
        v["lastCheckedAt"] = json!(t);
    }
    if let Some(e) = &row.last_error {
        v["lastError"] = json!(e);
    }
    if let Some(d) = &row.index_digest {
        v["indexDigest"] = json!(d);
    }
    v
}

/// The listings of `GET /v1/marketplaces/{id}/listings` with nothing installed.
pub fn listings_view(row: &MarketplaceRow) -> Value {
    listings_view_with(row, &[])
}

/// The listings of `GET /v1/marketplaces/{id}/listings`: what the last good read found, and for
/// each listing what is installed (`installed: { ref, commit, installedAt }` or null) and whether
/// the index now names another tag that exists and can be read (`updateAvailable`). A disabled
/// marketplace shows none. A read never installs anything (M1).
pub fn listings_view_with(row: &MarketplaceRow, installs: &[installs::InstallRow]) -> Value {
    if !row.enabled {
        return json!([]);
    }
    Value::Array(
        row.listings
            .iter()
            .map(|l| {
                let installed = installs.iter().find(|i| i.listing_id == l.id);
                let update_available = installed.is_some_and(|i| {
                    i.git_ref != l.git_ref && l.commit.is_some() && l.error.is_none()
                });
                let mut v = json!({
                    "id": l.id,
                    "displayName": l.display_name,
                    "description": l.description,
                    "ref": l.git_ref,
                    "requiresNode": l.requires_node,
                    "artifacts": l.artifacts,
                    "installed": installed.map_or(Value::Null, |i| json!({
                        "ref": i.git_ref,
                        "commit": i.commit,
                        "installedAt": i.installed_at,
                    })),
                    "updateAvailable": update_available,
                });
                if let Some(c) = &l.connector {
                    v["connector"] = json!(c);
                }
                if let Some(c) = &l.commit {
                    v["commit"] = json!(c);
                }
                if let Some(s) = &l.summary {
                    v["summary"] = s.clone();
                }
                if let Some(e) = &l.error {
                    v["error"] = json!(e);
                }
                v
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests;
