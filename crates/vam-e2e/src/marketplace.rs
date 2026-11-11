//! Git marketplaces made by the runner, and the steps that talk to the node about them.
//!
//! Piece: `git-marketplace` (a fixture directory committed, tagged and served as a bare repo).
//! Steps: `git-tag`, `marketplace-add`, `marketplace-refresh`, `marketplace-install`,
//! `marketplace-update`, `catalog-write`; expects: `marketplaces`, `listings`, `catalog`. The wire
//! shapes are written down in `e2e/README.md` under "Marketplace".
//!
//! The repositories are deterministic: fixed author, committer and dates, so the same fixture
//! gives the same commit SHAs in every scenario of a run (a node that remembers a tag's commit
//! must not see it move between scenarios). This module is a child of `engine`.

use super::{conn, Engine, Fail};
use crate::api::seg;
use crate::scenario::Class;
use crate::world::{PieceKind, WorldPiece};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The id the node gives its preconfigured Squrell marketplace.
pub const PRECONFIGURED_ID: &str = "squrell";
/// The first commit's time (seconds since the epoch); each later commit adds one second.
const EPOCH: u64 = 1_767_225_600;

// ---- step and expect shapes ------------------------------------------------------------------

/// `git-tag: { piece, tag, move, overlay }`: adds a tag to the marketplace repo. With `overlay`
/// (a fixture directory under `e2e/world/fixtures`) its files are copied over the work tree and
/// committed first, so the tag names a new commit. `move: true` lets an existing tag move.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTag {
    pub piece: String,
    pub tag: String,
    #[serde(default, rename = "move")]
    pub move_: bool,
    pub overlay: Option<String>,
}

/// `marketplace-add: { piece, as, refused, contains }`: `POST /v1/marketplaces` with the piece's
/// `url` and `publisherDomain`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketplaceAdd {
    pub piece: String,
    #[serde(rename = "as")]
    pub as_: Option<String>,
    #[serde(default)]
    pub refused: bool,
    pub contains: Option<String>,
}

/// `marketplace-refresh | marketplace-install | marketplace-update`.
/// `marketplace` is the alias of a `git-marketplace` piece; `listing` is needed by install and update.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketplaceAct {
    pub marketplace: String,
    pub listing: Option<String>,
    #[serde(rename = "as")]
    pub as_: Option<String>,
    #[serde(default)]
    pub refused: bool,
    pub contains: Option<String>,
}

/// `catalog-write: { kind, urn, action, as, refused, contains }`: a write to an artifact by URN.
/// `refused` defaults to true: the point is that nobody may change an installed row.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogWrite {
    /// `agent`, `playbook` or `skill`.
    pub kind: String,
    pub urn: String,
    /// `put` (agent, playbook), `release` or `delete`.
    pub action: String,
    #[serde(rename = "as")]
    pub as_: Option<String>,
    #[serde(default = "yes")]
    pub refused: bool,
    pub contains: Option<String>,
}

fn yes() -> bool {
    true
}

/// `marketplaces: { marketplace, as, fields, absent }`: one entry of `GET /v1/marketplaces`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketplacesExpect {
    pub marketplace: String,
    #[serde(rename = "as")]
    pub as_: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
    #[serde(default)]
    pub absent: Vec<String>,
}

/// `listings: { marketplace, listing, as, fields, absent }`: one entry of
/// `GET /v1/marketplaces/{id}/listings`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListingsExpect {
    pub marketplace: String,
    pub listing: String,
    #[serde(rename = "as")]
    pub as_: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
    #[serde(default)]
    pub absent: Vec<String>,
}

/// `catalog: { kind, urnPrefix, as, count, fields }`: the rows of `GET /v1/{agents|playbooks|skills}`
/// whose URN starts with `urnPrefix`; `fields` must hold on every one of them.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatalogExpect {
    pub kind: String,
    pub urn_prefix: String,
    #[serde(rename = "as")]
    pub as_: Option<String>,
    pub count: Option<usize>,
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
}

// ---- the repositories ------------------------------------------------------------------------

/// What the steps remember inside a scenario.
#[derive(Debug, Default)]
pub struct MktState {
    /// Piece alias -> the id the node gave the marketplace an admin added.
    ids: BTreeMap<String, String>,
    /// Marketplaces the steps added: `(id, key)`, removed at cleanup as the same identity.
    added: Vec<(String, Option<String>)>,
    /// The node's marketplace state was reset for this scenario.
    reset_done: bool,
}

/// The process-only route that returns the node's marketplace state to its boot state.
pub const PROCESS_RESET_PATH: &str = "/v1/process/marketplace-reset";

/// Where a piece's repositories live: `<root>/<piece>/{work,bare.git}`.
pub fn repo_dir(root: &Path, piece: &str) -> PathBuf {
    root.join(piece)
}

/// `file://` URL of the bare repo of a piece.
pub fn bare_url(root: &Path, piece: &str) -> String {
    format!(
        "file://{}",
        repo_dir(root, piece).join("bare.git").display()
    )
}

/// The seam's value: the https URL of each `git-marketplace` piece mapped to its bare repo.
pub fn seam_json<'a>(pieces: impl Iterator<Item = &'a WorldPiece>, root: &Path) -> String {
    let mut m = serde_json::Map::new();
    for p in pieces.filter(|p| p.kind == PieceKind::GitMarketplace) {
        if let Some(url) = &p.url {
            m.insert(url.clone(), json!(bare_url(root, &p.piece)));
        }
    }
    Value::Object(m).to_string()
}

fn git(dir: &Path, home: &Path, n: u64, args: &[&str]) -> Result<String, String> {
    let when = format!("@{} +0000", EPOCH + n);
    let out = Command::new("git")
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "e2e")
        .env("GIT_AUTHOR_EMAIL", "e2e@example.test")
        .env("GIT_COMMITTER_NAME", "e2e")
        .env("GIT_COMMITTER_EMAIL", "e2e@example.test")
        .env("GIT_AUTHOR_DATE", &when)
        .env("GIT_COMMITTER_DATE", &when)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(["-c", "tag.gpgsign=false"])
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    for e in std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))? {
        let e = e.map_err(|e| e.to_string())?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let ty = e.file_type().map_err(|e| e.to_string())?;
        if ty.is_dir() {
            copy_tree(&src, &dst)?;
        } else if ty.is_file() {
            std::fs::copy(&src, &dst).map_err(|e| format!("{}: {e}", src.display()))?;
        }
    }
    Ok(())
}

/// Makes (or makes again) the repository of a piece: copies `fixture`, commits it on `branch`
/// with the given tags and clones it bare. Anything an earlier scenario left is removed first.
pub fn build_repo(
    root: &Path,
    piece: &str,
    fixture: &Path,
    branch: &str,
    tags: &[String],
) -> Result<(), String> {
    let base = repo_dir(root, piece);
    let _ = std::fs::remove_dir_all(&base);
    let (work, home) = (base.join("work"), base.join("home"));
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    copy_tree(fixture, &work)?;
    git(&work, &home, 0, &["init", "-q", "-b", branch])?;
    git(&work, &home, 0, &["add", "-A"])?;
    git(&work, &home, 0, &["commit", "-q", "-m", "marketplace"])?;
    for t in tags {
        git(&work, &home, 0, &["tag", t])?;
    }
    git(
        &base,
        &home,
        0,
        &["clone", "-q", "--bare", "work", "bare.git"],
    )?;
    Ok(())
}

/// Adds or moves a tag; with an overlay, on a new commit. Pushes the branch and the tag to the
/// bare repo.
pub fn tag_repo(
    root: &Path,
    piece: &str,
    branch: &str,
    t: &GitTag,
    overlay: Option<&Path>,
) -> Result<(), String> {
    let base = repo_dir(root, piece);
    let (work, home) = (base.join("work"), base.join("home"));
    if !work.is_dir() {
        return Err(format!("no repository for `{piece}` (is it in `given`?)"));
    }
    if let Some(o) = overlay {
        copy_tree(o, &work)?;
        let n: u64 = git(&work, &home, 0, &["rev-list", "--count", "HEAD"])?
            .parse()
            .unwrap_or(1);
        git(&work, &home, n, &["add", "-A"])?;
        git(
            &work,
            &home,
            n,
            &["commit", "-q", "-m", &format!("update {n}")],
        )?;
    }
    let exists = git(
        &work,
        &home,
        0,
        &[
            "rev-parse",
            "-q",
            "--verify",
            &format!("refs/tags/{}", t.tag),
        ],
    )
    .is_ok();
    if exists && !t.move_ {
        return Err(format!("tag `{}` exists (use `move: true`)", t.tag));
    }
    git(&work, &home, 0, &["tag", "-f", &t.tag])?;
    let refspec_branch = format!("HEAD:refs/heads/{branch}");
    let refspec_tag = format!("refs/tags/{0}:refs/tags/{0}", t.tag);
    git(
        &work,
        &home,
        0,
        &[
            "push",
            "-q",
            "--force",
            "../bare.git",
            &refspec_branch,
            &refspec_tag,
        ],
    )?;
    Ok(())
}

// ---- engine side -----------------------------------------------------------------------------

fn product(want: impl Into<String>, got: impl Into<String>) -> Fail {
    Fail::new(Class::Product, want, got)
}

fn dotted<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |cur, k| match cur {
        Value::Array(a) => k.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => cur.get(k),
    })
}

fn collection(kind: &str) -> Result<&'static str, Fail> {
    match kind {
        "agent" => Ok("agents"),
        "playbook" => Ok("playbooks"),
        "skill" => Ok("skills"),
        other => Err(Fail::new(
            Class::Scenario,
            "kind agent, playbook or skill",
            other.to_string(),
        )),
    }
}

impl Engine {
    /// Sets the directory the repositories are made under (call before `prepare`).
    pub fn set_marketplace_root(&mut self, root: PathBuf) {
        self.marketplace_root = Some(root);
    }

    fn mkt_root(&self) -> Result<PathBuf, Fail> {
        self.marketplace_root.clone().ok_or_else(|| {
            Fail::new(
                Class::Harness,
                "the runner chose a directory for marketplace repositories",
                "none set",
            )
        })
    }

    /// A `git-marketplace` piece: makes its repository.
    pub(super) fn start_git_marketplace_piece(&mut self, wp: &WorldPiece) -> Result<(), Fail> {
        let root = self.mkt_root()?;
        let Some(fixture) = wp.fixture_dir.clone().filter(|d| d.is_dir()) else {
            return Err(Fail::new(
                Class::Harness,
                format!("fixture directory of piece `{}`", wp.piece),
                "missing",
            ));
        };
        // One vamd serves the whole run, so a scenario starts from the node's boot state for
        // marketplaces: nothing installed, the preconfigured entry unread. Once per scenario.
        if !self.ctx.mkt.reset_done {
            self.reset_marketplace_node()?;
            self.ctx.mkt.reset_done = true;
        }
        build_repo(&root, &wp.piece, &fixture, "main", &wp.tags)
            .map_err(|e| Fail::new(Class::Harness, format!("repository of `{}`", wp.piece), e))
    }

    /// `POST /v1/process/marketplace-reset` as the bootstrap key: the process-only seam that
    /// removes every marketplace install and the preconfigured entry's read state, so scenarios
    /// give the same result alone, in a batch and in any order. The default vamd has no such route
    /// (the release gate proves it).
    fn reset_marketplace_node(&mut self) -> Result<(), Fail> {
        let r = self
            .api
            .post_as(None, PROCESS_RESET_PATH, &json!({}))
            .map_err(conn)?;
        if r.ok() {
            Ok(())
        } else {
            Err(Fail::new(
                Class::Harness,
                format!("POST {PROCESS_RESET_PATH} resets the node's marketplace state"),
                format!(
                    "{} {}",
                    r.status,
                    self.scrub(&r.body.chars().take(200).collect::<String>())
                ),
            ))
        }
    }

    pub(super) fn git_tag(&mut self, t: &GitTag) -> Result<(), Fail> {
        let root = self.mkt_root()?;
        let wp = self.piece_def(&t.piece)?;
        let overlay = match &t.overlay {
            Some(o) => Some(
                wp.fixture_dir
                    .as_ref()
                    .and_then(|d| d.parent())
                    .map(|d| d.join(o))
                    .filter(|d| d.is_dir())
                    .ok_or_else(|| {
                        Fail::new(
                            Class::Scenario,
                            format!("overlay fixture `{o}`"),
                            "not a directory under e2e/world/fixtures",
                        )
                    })?,
            ),
            None => None,
        };
        tag_repo(&root, &wp.piece, "main", t, overlay.as_deref())
            .map_err(|e| Fail::new(Class::Harness, format!("git-tag `{}`", t.tag), e))
    }

    /// The world piece an alias in `given` stands for.
    fn piece_def(&self, alias: &str) -> Result<WorldPiece, Fail> {
        let wp = self
            .ctx
            .pieces
            .get(alias)
            .and_then(|r| self.pieces.get(&r.piece))
            .filter(|w| w.kind == PieceKind::GitMarketplace);
        wp.cloned().ok_or_else(|| {
            Fail::new(
                Class::Scenario,
                format!("git-marketplace alias `{alias}`"),
                "not a git-marketplace piece in `given`",
            )
        })
    }

    fn mkt_id(&self, alias: &str) -> Result<String, Fail> {
        let wp = self.piece_def(alias)?;
        if wp.preconfigured {
            return Ok(PRECONFIGURED_ID.to_string());
        }
        self.ctx.mkt.ids.get(alias).cloned().ok_or_else(|| {
            Fail::new(
                Class::Scenario,
                format!("`marketplace-add` for `{alias}` before it is used"),
                "no id yet",
            )
        })
    }

    fn mkt_judge(
        &self,
        step: &str,
        refused: bool,
        contains: Option<&str>,
        r: &crate::api::Reply,
    ) -> Result<(), Fail> {
        let shown = format!(
            "{} {}",
            r.status,
            self.scrub(&r.body.chars().take(300).collect::<String>())
        );
        if refused {
            if !matches!(r.status, 400 | 403 | 409) {
                return Err(product(
                    format!("{step} is refused (400, 403 or 409)"),
                    shown,
                ));
            }
            if let Some(c) = contains.filter(|c| !r.body.contains(*c)) {
                return Err(product(format!("{step} is refused with `{c}`"), shown));
            }
            return Ok(());
        }
        if r.ok() {
            Ok(())
        } else {
            Err(product(format!("{step} is accepted"), shown))
        }
    }

    pub(super) fn marketplace_add(&mut self, a: &MarketplaceAdd) -> Result<(), Fail> {
        let key = self.key_of(a.as_.as_deref())?;
        let wp = self.piece_def(&a.piece)?;
        let mut body = json!({ "url": wp.url });
        if let Some(d) = &wp.publisher_domain {
            body["publisherDomain"] = json!(d);
        }
        let r = self
            .api
            .post_as(key.as_deref(), "/v1/marketplaces", &body)
            .map_err(conn)?;
        if r.ok() {
            let j = r.json();
            let id = j["marketplace"]["id"]
                .as_str()
                .or_else(|| j["id"].as_str())
                .unwrap_or_default()
                .to_string();
            if !id.is_empty() {
                self.ctx.mkt.ids.insert(a.piece.clone(), id.clone());
                self.ctx.mkt.added.push((id, key.clone()));
            }
        }
        self.mkt_judge(
            "POST /v1/marketplaces",
            a.refused,
            a.contains.as_deref(),
            &r,
        )
    }

    /// `kind` is `refresh`, `install` or `update`.
    pub(super) fn marketplace_act(&mut self, kind: &str, a: &MarketplaceAct) -> Result<(), Fail> {
        let key = self.key_of(a.as_.as_deref())?;
        let id = self.mkt_id(&a.marketplace)?;
        let path = match (kind, &a.listing) {
            ("refresh", _) => format!("/v1/marketplaces/{}/refresh", seg(&id)),
            (k, Some(l)) => format!("/v1/marketplaces/{}/listings/{}/{k}", seg(&id), seg(l)),
            (k, None) => {
                return Err(Fail::new(
                    Class::Scenario,
                    format!("`listing` on marketplace-{k}"),
                    "none",
                ))
            }
        };
        let r = self
            .api
            .post_as(key.as_deref(), &path, &json!({}))
            .map_err(conn)?;
        self.mkt_judge(
            &format!("POST {path}"),
            a.refused,
            a.contains.as_deref(),
            &r,
        )
    }

    pub(super) fn catalog_write(&mut self, w: &CatalogWrite) -> Result<(), Fail> {
        let key = self.key_of(w.as_.as_deref())?;
        let coll = collection(&w.kind)?;
        let urn = seg(&w.urn);
        let (step, r) = match (w.action.as_str(), coll) {
            ("put", "skills") => {
                return Err(Fail::new(
                    Class::Scenario,
                    "action put on an agent or a playbook",
                    "a skill is published as a package",
                ))
            }
            ("put", c) => {
                let body = json!({ "spec": { "resourceId": { "urn": w.urn },
                    "displayName": "overwritten", "description": "overwritten" } });
                let p = format!("/v1/{c}");
                (
                    format!("PUT {p}"),
                    self.api.put_as(key.as_deref(), &p, &body).map_err(conn)?,
                )
            }
            ("release", c) => {
                let base = if c == "skills" {
                    format!("/v1/skills/packages/{urn}")
                } else {
                    format!("/v1/{c}/{urn}")
                };
                let p = format!("{base}/release");
                let body = json!({ "version": { "major": 9, "minor": 0, "patch": 0 } });
                (
                    format!("POST {p}"),
                    self.api.post_as(key.as_deref(), &p, &body).map_err(conn)?,
                )
            }
            ("delete", c) => {
                let p = if c == "skills" {
                    format!("/v1/skills/packages/{urn}")
                } else {
                    format!("/v1/{c}/{urn}")
                };
                (
                    format!("DELETE {p}"),
                    self.api.delete_as(key.as_deref(), &p).map_err(conn)?,
                )
            }
            (other, _) => {
                return Err(Fail::new(
                    Class::Scenario,
                    "action put, release or delete",
                    other.to_string(),
                ))
            }
        };
        self.mkt_judge(&step, w.refused, w.contains.as_deref(), &r)
    }

    /// Fields and absent paths must hold on `entry`.
    fn mkt_fields(
        &self,
        what: &str,
        entry: &Value,
        fields: &BTreeMap<String, Value>,
        absent: &[String],
    ) -> Result<(), Fail> {
        for (path, want) in fields {
            let got = dotted(entry, path);
            // `{ present: true }` needs a non-empty value; `{ contains: x }` a string holding x.
            if let Some(m) = want.as_object().filter(|m| m.len() == 1) {
                if let Some(p) = m.get("present").and_then(Value::as_bool) {
                    let has = got.is_some_and(|g| !g.is_null() && g != &json!(""));
                    if has == p {
                        continue;
                    }
                    return Err(product(
                        format!("{what}: {path} is {}", if p { "set" } else { "empty" }),
                        self.scrub(&format!("{got:?}")),
                    ));
                }
                if let Some(c) = m.get("contains").and_then(Value::as_str) {
                    if got.and_then(Value::as_str).is_some_and(|s| s.contains(c)) {
                        continue;
                    }
                    return Err(product(
                        format!("{what}: {path} contains `{c}`"),
                        self.scrub(&format!("{got:?}")),
                    ));
                }
            }
            if got != Some(want) {
                return Err(product(
                    format!("{what}: {path} is {want}"),
                    self.scrub(&format!("{got:?}")),
                ));
            }
        }
        for path in absent {
            if dotted(entry, path).is_some() {
                return Err(product(
                    format!("{what} has no {path}"),
                    "present".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn mkt_get(&self, key: Option<&str>, path: &str) -> Result<Value, Fail> {
        let r = self.api.get_as(key, path).map_err(conn)?;
        if !r.ok() {
            return Err(product(
                format!("GET {path} answers"),
                format!(
                    "{} {}",
                    r.status,
                    self.scrub(&r.body.chars().take(300).collect::<String>())
                ),
            ));
        }
        Ok(r.json())
    }

    fn list_in<'a>(body: &'a Value, key: &str) -> Vec<&'a Value> {
        body.get(key)
            .and_then(Value::as_array)
            .or_else(|| body.as_array())
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    }

    pub(super) fn expect_marketplaces(&mut self, x: &MarketplacesExpect) -> Result<(), Fail> {
        let key = self.key_of(x.as_.as_deref())?;
        let id = self.mkt_id(&x.marketplace)?;
        let body = self.mkt_get(key.as_deref(), "/v1/marketplaces")?;
        let list = Self::list_in(&body, "marketplaces");
        let Some(entry) = list.iter().find(|e| e["id"].as_str() == Some(id.as_str())) else {
            return Err(product(
                format!("marketplace `{id}` is listed"),
                self.scrub(&body.to_string().chars().take(300).collect::<String>()),
            ));
        };
        self.mkt_fields(&format!("marketplace `{id}`"), entry, &x.fields, &x.absent)
    }

    pub(super) fn expect_listings(&mut self, x: &ListingsExpect) -> Result<(), Fail> {
        let key = self.key_of(x.as_.as_deref())?;
        let id = self.mkt_id(&x.marketplace)?;
        let body = self.mkt_get(
            key.as_deref(),
            &format!("/v1/marketplaces/{}/listings", seg(&id)),
        )?;
        let list = Self::list_in(&body, "listings");
        let Some(entry) = list
            .iter()
            .find(|e| e["id"].as_str() == Some(x.listing.as_str()))
        else {
            return Err(product(
                format!("listing `{}` of `{id}` is listed", x.listing),
                self.scrub(&body.to_string().chars().take(300).collect::<String>()),
            ));
        };
        self.mkt_fields(
            &format!("listing `{}`", x.listing),
            entry,
            &x.fields,
            &x.absent,
        )
    }

    pub(super) fn expect_catalog(&mut self, x: &CatalogExpect) -> Result<(), Fail> {
        let key = self.key_of(x.as_.as_deref())?;
        let coll = collection(&x.kind)?;
        // The list routes: agents and playbooks answer `records`, skills are uploaded packages.
        let (path, list_key) = match coll {
            "skills" => ("/v1/skills/packages", "packages"),
            other => (
                if other == "agents" {
                    "/v1/agents"
                } else {
                    "/v1/playbooks"
                },
                "records",
            ),
        };
        let body = self.mkt_get(key.as_deref(), path)?;
        let urn_of = |e: &Value| -> String {
            ["urn", "resourceId.urn", "spec.resourceId.urn"]
                .iter()
                .find_map(|p| dotted(e, p).and_then(Value::as_str))
                .unwrap_or_default()
                .to_string()
        };
        // The list holds every release of an artifact; an artifact is counted once, at its newest
        // release (a draft has no release and is not counted).
        let version_of = |e: &Value| -> Option<(i64, i64, i64)> {
            let v = dotted(e, "version")?;
            if v.get("prerelease")
                .and_then(Value::as_str)
                .is_some_and(|p| !p.is_empty())
            {
                return None;
            }
            let n = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
            Some((n("major"), n("minor"), n("patch")))
        };
        let mut newest: BTreeMap<String, (&Value, (i64, i64, i64))> = BTreeMap::new();
        for e in Self::list_in(&body, list_key) {
            let urn = urn_of(e);
            if !urn.starts_with(&x.urn_prefix) {
                continue;
            }
            let Some(v) = version_of(e) else { continue };
            if newest.get(&urn).is_none_or(|(_, old)| v > *old) {
                newest.insert(urn, (e, v));
            }
        }
        let rows: Vec<&Value> = newest.values().map(|(e, _)| *e).collect();
        if let Some(n) = x.count.filter(|n| *n != rows.len()) {
            return Err(product(
                format!("{n} {coll} under {}", x.urn_prefix),
                format!("{} rows", rows.len()),
            ));
        }
        for row in rows {
            self.mkt_fields(&urn_of(row), row, &x.fields, &[])?;
        }
        Ok(())
    }

    /// Removes what the scenario installed (the process-only reset) and the marketplaces the
    /// steps added, as the identity that added them. The reset goes first: a marketplace with an
    /// install cannot be removed.
    pub(super) fn cleanup_marketplaces(&mut self) {
        if self.ctx.mkt.reset_done {
            if let Err(f) = self.reset_marketplace_node() {
                eprintln!("vam-e2e: marketplace cleanup: {}", f.observed);
            }
        }
        for (id, key) in std::mem::take(&mut self.ctx.mkt.added) {
            let _ = self
                .api
                .delete_as(key.as_deref(), &format!("/v1/marketplaces/{}", seg(&id)));
        }
        self.ctx.mkt = MktState::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &Path) {
        std::fs::create_dir_all(dir.join("listings")).unwrap();
        std::fs::write(dir.join("marketplace.json"), "{}").unwrap();
        std::fs::write(dir.join("listings/a.txt"), "a").unwrap();
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vam-e2e-mkt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn rev(root: &Path, piece: &str, r: &str) -> String {
        let b = repo_dir(root, piece);
        git(&b.join("bare.git"), &b.join("home"), 0, &["rev-parse", r]).unwrap()
    }

    #[test]
    fn repo_is_deterministic_and_tags_add_and_move() {
        let root = tmp("det");
        let fx = root.join("fx");
        fixture(&fx);
        let tags = vec!["v1".to_string()];
        build_repo(&root, "p", &fx, "main", &tags).unwrap();
        let first = rev(&root, "p", "refs/tags/v1");
        build_repo(&root, "p", &fx, "main", &tags).unwrap();
        assert_eq!(
            first,
            rev(&root, "p", "refs/tags/v1"),
            "same fixture, same commit"
        );

        let overlay = root.join("ov");
        std::fs::create_dir_all(&overlay).unwrap();
        std::fs::write(overlay.join("marketplace.json"), "{\"n\":1}").unwrap();
        let t = GitTag {
            piece: "p".into(),
            tag: "v2".into(),
            move_: false,
            overlay: None,
        };
        tag_repo(&root, "p", "main", &t, Some(&overlay)).unwrap();
        let v2 = rev(&root, "p", "refs/tags/v2");
        assert_ne!(v2, first);
        assert_eq!(v2, rev(&root, "p", "refs/heads/main"));
        // An existing tag moves only on request.
        let again = GitTag {
            tag: "v1".into(),
            ..t.clone()
        };
        assert!(tag_repo(&root, "p", "main", &again, None).is_err());
        let moved = GitTag {
            move_: true,
            ..again
        };
        tag_repo(&root, "p", "main", &moved, None).unwrap();
        assert_eq!(rev(&root, "p", "refs/tags/v1"), v2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn seam_maps_urls_to_bare_repos() {
        let p: WorldPiece = serde_yaml::from_str(
            "piece: m\nkind: git-marketplace\nfixture: m\nurl: https://example.test/m\n",
        )
        .unwrap();
        let s = seam_json([&p].into_iter(), Path::new("/x"));
        assert_eq!(s, r#"{"https://example.test/m":"file:///x/m/bare.git"}"#);
    }
}
