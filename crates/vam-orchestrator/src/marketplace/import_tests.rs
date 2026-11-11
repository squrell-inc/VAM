use super::*;
use crate::catalog_resolve::ResolveMode;
use crate::error::OrchestratorError as E;
use crate::marketplace;
use crate::marketplace::fetch::{self, Fetcher, Limits};
use crate::marketplace::testutil::{git_bin, TestRepo};
use crate::marketplace::{add_blocking, delete_blocking, refresh_blocking, MarketplaceConfig};
use crate::skill_packages::DRIVE9_ENV_LOCK;
use std::time::Duration;

const SQ_URL: &str = "https://github.com/squrell-inc/vam-marketplace";
const SQ_TAG: &str = "microsoft-365-v1.0.0";
const SKILL: &str = "urn:vam:skill:hub.squrell.cloud/squrell/m365-mail";
const AGENT: &str = "urn:vam:agent:hub.squrell.cloud/squrell/executive-assistant";
const PLAYBOOK: &str = "urn:vam:playbook:hub.squrell.cloud/squrell/morning-triage";
const COMMUNITY_URL: &str = "https://market.community.example/vam-marketplace";
const COMMUNITY_TAGS: [&str; 4] = [
    "helpful-notes-v1.0.0",
    "with-a-connector-v1.0.0",
    "claims-the-squrell-name-v1.0.0",
    "claims-the-org-name-v1.0.0",
];

/// A node, a drive9 root and a marketplace repository, for one test. Holds the env lock.
struct World {
    store: SqliteWorkflowStore,
    cfg: MarketplaceConfig,
    repo: TestRepo,
    drive: tempfile::TempDir,
    _state: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}

fn world(
    fixture: &str,
    tags: &[&str],
    url: &str,
    edit: impl FnOnce(&mut TestRepo),
) -> Option<World> {
    let lock = DRIVE9_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let git = git_bin()?;
    let state = tempfile::tempdir().unwrap();
    let drive = tempfile::tempdir().unwrap();
    std::env::set_var("VAM_DRIVE9_ROOT", drive.path());
    let cfg = MarketplaceConfig {
        fetcher: Fetcher::new(
            Some(git),
            state.path().join("marketplaces"),
            Limits::default(),
        ),
        refresh_every: Some(Duration::from_secs(3600)),
        node_version: "0.1.0".into(),
    };
    let mut repo = TestRepo::new(fixture, tags);
    edit(&mut repo);
    fetch::TEST_SEAM.with(|m| {
        m.borrow_mut().insert(url.to_string(), repo.url());
    });
    Some(World {
        store: SqliteWorkflowStore::in_memory().unwrap(),
        cfg,
        repo,
        drive,
        _state: state,
        _lock: lock,
    })
}

fn squrell(edit: impl FnOnce(&mut TestRepo)) -> Option<World> {
    let w = world("marketplace-squrell", &[SQ_TAG], SQ_URL, edit)?;
    refresh_blocking(&w.store, &w.cfg, "squrell").unwrap();
    Some(w)
}

impl World {
    fn install(&self, id: &str, listing: &str, mode: Mode) -> Result<Outcome, E> {
        install_blocking(&self.store, &self.cfg, id, listing, mode, None, "admin")
    }

    fn count(&self, table: &str, like: &str) -> i64 {
        let c = self.store.connection.lock().unwrap();
        c.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE urn LIKE ?1"),
            [like],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn versions(&self, table: &str, urn: &str) -> Vec<String> {
        let c = self.store.connection.lock().unwrap();
        let mut s = c
            .prepare(&format!(
                "SELECT major || '.' || minor || '.' || patch FROM {table}
                 WHERE urn = ?1 AND channel = 'released' ORDER BY major, minor, patch"
            ))
            .unwrap();
        s.query_map([urn], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn rows_of_marketplace_tier(&self) -> i64 {
        let c = self.store.connection.lock().unwrap();
        [
            "agents",
            "playbooks",
            "skills",
            "policies",
            "skill_packages",
        ]
        .iter()
        .map(|t| {
            c.query_row(
                &format!("SELECT COUNT(*) FROM {t} WHERE catalog_tier = 'marketplace'"),
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        })
        .sum()
    }

    /// The stored bytes of the newest release of `urn`, or of `version` (`X.Y.Z`).
    fn bytes(&self, table: &str, urn: &str, version: Option<&str>) -> Vec<u8> {
        let c = self.store.connection.lock().unwrap();
        c.query_row(
            &format!(
                "SELECT spec_bytes FROM {table}
                 WHERE urn = ?1 AND channel = 'released'
                   AND (?2 = '' OR major || '.' || minor || '.' || patch = ?2)
                 ORDER BY major DESC, minor DESC, patch DESC LIMIT 1"
            ),
            params![urn, version.unwrap_or("")],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn agent(&self, version: Option<&str>) -> AgentSpec {
        AgentSpec::decode(&*self.bytes("agents", AGENT, version)).unwrap()
    }

    fn playbook(&self) -> PlaybookSpec {
        PlaybookSpec::decode(&*self.bytes("playbooks", PLAYBOOK, None)).unwrap()
    }
}

#[test]
fn the_squrell_listing_installs_as_released_read_only_rows_with_their_origin() {
    let Some(w) = squrell(|_| {}) else { return };
    let commit = w.repo.rev(SQ_TAG);
    let out = w
        .install("squrell", "microsoft-365", Mode::Install)
        .unwrap();
    assert!(out.changed);
    assert_eq!(
        (out.git_ref.as_str(), out.commit.as_str()),
        (SQ_TAG, commit.as_str())
    );
    let kinds: Vec<&str> = out.artifacts.iter().map(|a| a.kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["skill", "playbook", "agent"],
        "skills, then playbooks, then agents"
    );
    assert!(out.artifacts.iter().all(|a| a.version == "1.0.0"));

    // Released, owned by the marketplace, in the marketplace tier.
    for (table, urn) in [
        ("skill_packages", SKILL),
        ("playbooks", PLAYBOOK),
        ("agents", AGENT),
    ] {
        let c = w.store.connection.lock().unwrap();
        let (tier, owner, channel): (String, String, String) = c
            .query_row(
                &format!("SELECT catalog_tier, owner_user_id, channel FROM {table} WHERE urn = ?1"),
                [urn],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (tier.as_str(), owner.as_str(), channel.as_str()),
            ("marketplace", "marketplace:squrell", "released"),
            "{table}"
        );
    }
    // The agent's skill ref is frozen to the skill the listing ships, version and digest.
    let skill_digest = out.artifacts[0].digest.clone();
    let agent = w.agent(None);
    let r = &agent.skill_refs[0];
    assert_eq!(
        r.version.as_ref().map(|v| (v.major, v.minor, v.patch)),
        Some((1, 0, 0))
    );
    assert_eq!(
        r.digest.as_ref().map(|d| d.hex_hash.as_str()),
        Some(skill_digest.as_str())
    );
    // It runs in permissions mode.
    assert!(agent.permissions.is_some());

    // The origin is recorded and shows on the records; the install is recorded.
    let mut records = w.store.list_agent_records_visible("local", false).unwrap();
    w.store.attach_agent_origins(&mut records).unwrap();
    let rec = records
        .iter()
        .find(|r| {
            r.spec
                .as_ref()
                .and_then(|s| s.resource_id.as_ref())
                .map(|i| i.urn.as_str())
                == Some(AGENT)
        })
        .expect("members see the released installed agent");
    assert_eq!((rec.tier.as_str(), rec.read_only), ("marketplace", true));
    let o = rec.origin.as_ref().expect("origin");
    assert_eq!(
        (
            o.marketplace.as_str(),
            o.listing.as_str(),
            o.r#ref.as_str(),
            o.commit.as_str()
        ),
        ("squrell", "microsoft-365", SQ_TAG, commit.as_str())
    );
    assert_eq!(o.package_digest.len(), 64);
    let row = w.store.marketplace("squrell").unwrap().unwrap();
    let installs = w.store.marketplace_installs("squrell").unwrap();
    let listing = &marketplace::listings_view_with(&row, &installs)[0];
    assert_eq!(listing["installed"]["ref"], SQ_TAG);
    assert_eq!(listing["installed"]["commit"], commit);
    assert_eq!(listing["updateAvailable"], false);
    // The owner is a system account, not a person.
    assert!(w
        .store
        .list_users()
        .unwrap()
        .iter()
        .all(|u| !u.user_id.starts_with("marketplace:")));

    // The same tag again is a no-op.
    let again = w
        .install("squrell", "microsoft-365", Mode::Install)
        .unwrap();
    assert!(!again.changed);
    assert_eq!(w.versions("agents", AGENT), ["1.0.0"]);
}

#[test]
fn the_installed_skill_unpacks_and_the_installed_agent_resolves_it() {
    let Some(w) = squrell(|_| {}) else { return };
    w.install("squrell", "microsoft-365", Mode::Install)
        .unwrap();
    let agent = w.agent(None);
    let session = tempfile::tempdir().unwrap();
    let staged = crate::skill_package_resolve::resolve_agent_skill_refs(
        &w.store,
        &agent,
        session.path(),
        None,
        "local",
        ResolveMode::Release,
    )
    .expect("the marketplace domain resolves for installed rows")
    .expect("a skill was staged");
    assert!(staged.join("m365-mail/SKILLS.md").is_file());
    assert!(staged.join("m365-mail/manifest.json").is_file());
    let _ = &w.drive;
}

#[test]
fn nobody_writes_an_installed_row_or_a_marketplace_domain() {
    let Some(w) = squrell(|_| {}) else { return };
    w.install("squrell", "microsoft-365", Mode::Install)
        .unwrap();
    let before = w.rows_of_marketplace_tier();

    // Every caller write on an installed URN is refused: draft, put, release, owner change.
    let base = w.agent(None);
    let spec = |urn: &str| {
        let mut s = base.clone();
        s.resource_id = Some(vam_spec::v1::ResourceId {
            urn: urn.to_string(),
        });
        s.description = "mine".into();
        s
    };
    for who in ["local", "marketplace:squrell", "operator"] {
        let e = w
            .store
            .publish_agent_draft_with_owner(&spec(AGENT), who)
            .unwrap_err();
        assert!(matches!(e, E::Conflict(_)), "{who}: {e:?}");
        let e = w
            .store
            .put_agent_released_with_owner(&spec(AGENT), who)
            .unwrap_err();
        assert!(matches!(e, E::Conflict(_)), "{who}: {e:?}");
    }
    let v = vam_spec::v1::Version {
        major: 9,
        ..Default::default()
    };
    assert!(matches!(
        w.store.release_agent(AGENT, &v),
        Err(E::Conflict(_))
    ));
    assert!(matches!(
        w.store.release_playbook(PLAYBOOK, &v),
        Err(E::Conflict(_))
    ));
    assert!(matches!(
        w.store.release_skill(SKILL, &v),
        Err(E::Conflict(_))
    ));
    assert!(matches!(
        w.store.release_skill_package(SKILL, &v),
        Err(E::Conflict(_))
    ));
    let e = w
        .store
        .reassign_catalog_owner("agent", AGENT, "local")
        .unwrap_err();
    assert!(matches!(e, E::Conflict(_)), "{e:?}");
    let e = w
        .store
        .reassign_catalog_owner(
            "agent",
            "urn:vam:agent:local.vam.dev/acme/x",
            "marketplace:squrell",
        )
        .unwrap_err();
    assert!(matches!(e, E::InvalidContract(_)), "{e:?}");

    // A new URN under the Squrell domain is refused too: the domain is the marketplace's.
    let other = "urn:vam:agent:hub.squrell.cloud/acme/lookalike";
    let e = w
        .store
        .publish_agent_draft_with_owner(&spec(other), "local")
        .unwrap_err();
    assert!(
        matches!(&e, E::InvalidContract(m) if m.contains("hub.squrell.cloud")),
        "{e:?}"
    );
    let mut pb = w.playbook();
    pb.resource_id = Some(vam_spec::v1::ResourceId {
        urn: "urn:vam:playbook:hub.squrell.cloud/acme/p".into(),
    });
    let e = w.store.put_playbook_with_owner(&pb, "local").unwrap_err();
    assert!(
        matches!(&e, E::InvalidContract(m) if m.contains("hub.squrell.cloud")),
        "{e:?}"
    );
    // The same playbook under the installed URN is refused as installed.
    pb.resource_id = Some(vam_spec::v1::ResourceId {
        urn: PLAYBOOK.into(),
    });
    assert!(matches!(
        w.store.put_playbook_with_owner(&pb, "local"),
        Err(E::Conflict(_))
    ));
    assert!(w
        .store
        .publish_policy_draft_with_owner(
            "urn:vam:policy:hub.squrell.cloud/acme/p",
            "permit(principal, action, resource);",
            "local"
        )
        .is_err());
    assert_eq!(w.rows_of_marketplace_tier(), before);
    assert_eq!(w.count("agents", "%lookalike"), 0);
}

#[test]
fn a_moved_tag_is_refused_and_nothing_is_written() {
    let Some(mut w) = squrell(|_| {}) else { return };
    // The tag is re-pointed at another commit after the node pinned it.
    w.repo.write(&[("CHANGED.txt", "x")]);
    w.repo.tag(SQ_TAG);
    let e = w
        .install("squrell", "microsoft-365", Mode::Install)
        .unwrap_err();
    assert!(matches!(&e, E::Conflict(m) if m.contains("moved")), "{e:?}");
    assert_eq!(w.rows_of_marketplace_tier(), 0);
    assert!(w.store.marketplace_installs("squrell").unwrap().is_empty());
    // A refresh now refuses it as well, and the listing then names why.
    let r = refresh_blocking(&w.store, &w.cfg, "squrell").unwrap();
    assert!(r.refused.as_deref().unwrap().contains("moved"));
    let e = w
        .install("squrell", "microsoft-365", Mode::Install)
        .unwrap_err();
    assert!(e.to_string().contains("moved"), "{e:?}");
}

#[test]
fn a_newer_tag_updates_and_the_old_release_stays_resolvable() {
    let Some(mut w) = squrell(|_| {}) else { return };
    w.install("squrell", "microsoft-365", Mode::Install)
        .unwrap();
    let skill_v1 = w.agent(None).skill_refs[0].clone();
    // The marketplace tags a newer release and its index names it.
    w.repo.overlay("marketplace-squrell-update");
    w.repo.tag("microsoft-365-v1.1.0");
    refresh_blocking(&w.store, &w.cfg, "squrell").unwrap();
    let row = w.store.marketplace("squrell").unwrap().unwrap();
    let installs = w.store.marketplace_installs("squrell").unwrap();
    let l = &marketplace::listings_view_with(&row, &installs)[0];
    assert_eq!(l["updateAvailable"], true);
    assert_eq!(l["installed"]["ref"], SQ_TAG);
    // A refresh installed nothing.
    assert_eq!(w.versions("playbooks", PLAYBOOK), ["1.0.0"]);
    // Installing again is not an update.
    let e = w
        .install("squrell", "microsoft-365", Mode::Install)
        .unwrap_err();
    assert!(
        matches!(&e, E::Conflict(m) if m.contains("update")),
        "{e:?}"
    );

    let out = w.install("squrell", "microsoft-365", Mode::Update).unwrap();
    assert!(out.changed);
    assert_eq!(out.git_ref, "microsoft-365-v1.1.0");
    // The changed playbook is a new release; the unchanged skill and agent are kept as they were.
    assert_eq!(w.versions("playbooks", PLAYBOOK), ["1.0.0", "1.0.1"]);
    assert_eq!(w.versions("skill_packages", SKILL), ["1.0.0"]);
    assert_eq!(w.versions("agents", AGENT), ["1.0.0"]);
    // Every release keeps its own origin; the unchanged ones follow the new tag.
    let c = w.store.connection.lock().unwrap();
    let refs: Vec<(String, String, String)> = c
        .prepare("SELECT kind, major || '.' || minor || '.' || patch, git_ref FROM catalog_origins ORDER BY kind, major, minor, patch")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    drop(c);
    let want = |k: &str, v: &str, t: &str| (k.to_string(), v.to_string(), t.to_string());
    assert_eq!(
        refs,
        [
            want("agent", "1.0.0", "microsoft-365-v1.1.0"),
            want("playbook", "1.0.0", SQ_TAG),
            want("playbook", "1.0.1", "microsoft-365-v1.1.0"),
            want("skill", "1.0.0", "microsoft-365-v1.1.0"),
        ]
    );
    // An agent pinned to the first skill release still resolves after the update.
    let mut pinned = w.agent(Some("1.0.0"));
    pinned.skill_refs = vec![skill_v1];
    let session = tempfile::tempdir().unwrap();
    assert!(crate::skill_package_resolve::resolve_agent_skill_refs(
        &w.store,
        &pinned,
        session.path(),
        None,
        "local",
        ResolveMode::Release
    )
    .unwrap()
    .is_some());
    let row = w.store.marketplace("squrell").unwrap().unwrap();
    let installs = w.store.marketplace_installs("squrell").unwrap();
    let l = &marketplace::listings_view_with(&row, &installs)[0];
    assert_eq!(l["updateAvailable"], false);
    assert_eq!(l["installed"]["ref"], "microsoft-365-v1.1.0");
}

#[test]
fn an_update_names_the_artifacts_whose_capabilities_changed() {
    let Some(mut w) = squrell(|_| {}) else { return };
    let first = w
        .install("squrell", "microsoft-365", Mode::Install)
        .unwrap();
    assert!(first.capabilities_changed.is_empty());
    assert!(first.artifacts.iter().all(|a| a.summary_digest.len() == 64));
    // The new tag only rewords the playbook: what the artifacts can do is the same.
    w.repo.overlay("marketplace-squrell-update");
    w.repo.tag("microsoft-365-v1.1.0");
    refresh_blocking(&w.store, &w.cfg, "squrell").unwrap();
    let out = w.install("squrell", "microsoft-365", Mode::Update).unwrap();
    assert!(
        out.capabilities_changed.is_empty(),
        "{:?}",
        out.capabilities_changed
    );
    // A later tag lets the agent use one more tool: that artifact is named.
    let agent_json = std::fs::read_to_string(
        w.repo
            .work_dir()
            .join("listings/microsoft-365/agents/executive-assistant/agent.json"),
    )
    .unwrap()
    .replace(
        "\"target\": \"builtin:load_skill\"",
        "\"target\": \"builtin:write_file\"",
    );
    w.repo.write(&[(
        "listings/microsoft-365/agents/executive-assistant/agent.json",
        &agent_json,
    )]);
    let index = std::fs::read_to_string(w.repo.work_dir().join("marketplace.json"))
        .unwrap()
        .replace("microsoft-365-v1.1.0", "microsoft-365-v1.2.0");
    w.repo.write(&[("marketplace.json", &index)]);
    w.repo.tag("microsoft-365-v1.2.0");
    refresh_blocking(&w.store, &w.cfg, "squrell").unwrap();
    let out = w.install("squrell", "microsoft-365", Mode::Update).unwrap();
    assert_eq!(out.capabilities_changed, [AGENT]);
    assert_eq!(w.versions("agents", AGENT), ["1.0.0", "1.0.1"]);
}

#[test]
fn an_update_needs_an_install_and_an_install_needs_the_tag_the_index_names() {
    let Some(w) = squrell(|_| {}) else { return };
    let e = w
        .install("squrell", "microsoft-365", Mode::Update)
        .unwrap_err();
    assert!(
        matches!(&e, E::Conflict(m) if m.contains("not installed")),
        "{e:?}"
    );
    let e = install_blocking(
        &w.store,
        &w.cfg,
        "squrell",
        "microsoft-365",
        Mode::Install,
        Some("other-tag"),
        "admin",
    )
    .unwrap_err();
    assert!(
        matches!(&e, E::Conflict(m) if m.contains("other-tag")),
        "{e:?}"
    );
    let e = w.install("squrell", "nope", Mode::Install).unwrap_err();
    assert!(matches!(e, E::NotFound(_)), "{e:?}");
    let e = w
        .install("nope", "microsoft-365", Mode::Install)
        .unwrap_err();
    assert!(matches!(e, E::NotFound(_)), "{e:?}");
    w.store.set_marketplace_enabled("squrell", false).unwrap();
    let e = w
        .install("squrell", "microsoft-365", Mode::Install)
        .unwrap_err();
    assert!(matches!(e, E::Conflict(_)), "{e:?}");
    assert_eq!(w.rows_of_marketplace_tier(), 0);
}

#[test]
fn a_listing_whose_artifacts_refer_outside_the_listing_installs_nothing() {
    let broken = |r: &mut TestRepo| {
        let agent = std::fs::read_to_string(
            r.work_dir()
                .join("listings/microsoft-365/agents/executive-assistant/agent.json"),
        )
        .unwrap()
        .replace(
            "urn:vam:skill:hub.squrell.cloud/squrell/m365-mail",
            "urn:vam:skill:local.vam.dev/acme/finance-secrets",
        );
        r.write(&[(
            "listings/microsoft-365/agents/executive-assistant/agent.json",
            &agent,
        )]);
        r.tag(SQ_TAG);
    };
    let Some(w) = squrell(broken) else { return };
    let e = w
        .install("squrell", "microsoft-365", Mode::Install)
        .unwrap_err();
    assert!(
        matches!(&e, E::InvalidContract(m) if m.contains("does not ship")),
        "{e:?}"
    );
    // One transaction: the skill and the playbook, checked before the agent, left no row either.
    assert_eq!(w.rows_of_marketplace_tier(), 0);
    assert!(w.store.marketplace_installs("squrell").unwrap().is_empty());
    assert_eq!(w.count("catalog_origins", "%"), 0);
}

#[test]
fn a_listing_that_changes_an_installed_skill_without_a_new_version_is_refused() {
    let Some(mut w) = squrell(|_| {}) else { return };
    w.install("squrell", "microsoft-365", Mode::Install)
        .unwrap();
    let skills_md = std::fs::read_to_string(
        w.repo
            .work_dir()
            .join("listings/microsoft-365/skills/m365-mail/SKILLS.md"),
    )
    .unwrap();
    w.repo.write(&[(
        "listings/microsoft-365/skills/m365-mail/SKILLS.md",
        &format!("{skills_md}\nMore words.\n"),
    )]);
    w.repo.overlay("marketplace-squrell-update");
    w.repo.tag("microsoft-365-v1.1.0");
    refresh_blocking(&w.store, &w.cfg, "squrell").unwrap();
    let e = w
        .install("squrell", "microsoft-365", Mode::Update)
        .unwrap_err();
    assert!(
        matches!(&e, E::Conflict(m) if m.contains("different content")),
        "{e:?}"
    );
    // Still at the first tag, nothing half-written.
    assert_eq!(w.versions("playbooks", PLAYBOOK), ["1.0.0"]);
    let i = w
        .store
        .marketplace_install("squrell", "microsoft-365")
        .unwrap()
        .unwrap();
    assert_eq!(i.git_ref, SQ_TAG);
}

#[test]
fn org_content_under_the_same_urn_is_never_overwritten() {
    let Some(w) = squrell(|_| {}) else { return };
    // An org row at an installable URN (the domain was open to callers before this feature).
    {
        let c = w.store.connection.lock().unwrap();
        c.execute(
            "INSERT INTO playbooks (urn, major, minor, patch, prerelease, channel, revision, digest,
                 spec_bytes, created_at, updated_at, owner_user_id)
             VALUES (?1, 1, 0, 0, '', 'released', 1, 'abc', x'00', 1, 1, 'local')",
            [PLAYBOOK],
        )
        .unwrap();
    }
    let e = w
        .install("squrell", "microsoft-365", Mode::Install)
        .unwrap_err();
    assert!(
        matches!(&e, E::Conflict(m) if m.contains("already exists in this org")),
        "{e:?}"
    );
    assert_eq!(w.rows_of_marketplace_tier(), 0);
}

// ---- an added (Unverified) marketplace ---------------------------------------------------------

fn community() -> Option<(World, String)> {
    let w = world(
        "marketplace-community",
        &COMMUNITY_TAGS,
        COMMUNITY_URL,
        |_| {},
    )?;
    let row = add_blocking(
        &w.store,
        &w.cfg,
        COMMUNITY_URL,
        "market.community.example",
        "admin",
    )
    .unwrap();
    Some((w, row.id))
}

#[test]
fn an_unverified_listing_installs_under_its_own_domain_and_the_bad_ones_are_refused() {
    let Some((w, id)) = community() else { return };
    let out = w.install(&id, "helpful-notes", Mode::Install).unwrap();
    assert_eq!(out.artifacts.len(), 3);
    assert!(out
        .artifacts
        .iter()
        .all(|a| a.urn.contains(":market.community.example/")));
    let c = w.store.connection.lock().unwrap();
    let owner: String = c
        .query_row(
            "SELECT owner_user_id FROM agents WHERE urn LIKE '%market.community.example%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(c);
    assert_eq!(owner, format!("marketplace:{id}"));

    // M4: a connector in a listing of an added marketplace.
    let e = w
        .install(&id, "with-a-connector", Mode::Install)
        .unwrap_err();
    assert!(
        matches!(&e, E::InvalidContract(m) if m.contains("connector")),
        "{e:?}"
    );
    // M3: an artifact under the Squrell domain, or under the org's registry domain.
    let e = w
        .install(&id, "claims-the-squrell-name", Mode::Install)
        .unwrap_err();
    assert!(e.to_string().contains("hub.squrell.cloud"), "{e:?}");
    let e = w
        .install(&id, "claims-the-org-name", Mode::Install)
        .unwrap_err();
    assert!(e.to_string().contains("local.vam.dev"), "{e:?}");
    assert_eq!(w.count("agents", "urn:vam:agent:hub.squrell.cloud/%"), 0);
    assert_eq!(
        w.count("agents", "urn:vam:agent:local.vam.dev/community/%"),
        0
    );
    assert_eq!(w.rows_of_marketplace_tier(), 3);

    // The added marketplace's domain is now the marketplace's: callers cannot publish under it.
    let mut spec = AgentSpec::decode(
        w.bytes(
            "agents",
            "urn:vam:agent:market.community.example/community/note-helper",
            None,
        )
        .as_slice(),
    )
    .unwrap();
    spec.resource_id = Some(vam_spec::v1::ResourceId {
        urn: "urn:vam:agent:market.community.example/acme/mine".into(),
    });
    let e = w
        .store
        .publish_agent_draft_with_owner(&spec, "local")
        .unwrap_err();
    assert!(
        matches!(&e, E::InvalidContract(m) if m.contains("market.community.example")),
        "{e:?}"
    );
    // ... and a skill under it resolves for an installed agent, not for a caller's publish.
    let trusted = w.store.marketplace_publisher_domains().unwrap();
    let domains = crate::registry::registry_domains();
    assert!(domains.admits_installed_skill_registry("market.community.example", &trusted));
    assert!(!domains.admits_skill_registry("market.community.example"));

    // It cannot be removed while something from it is installed.
    let e = delete_blocking(&w.store, &w.cfg, &id).unwrap_err();
    assert!(
        matches!(&e, E::Conflict(m) if m.contains("installed")),
        "{e:?}"
    );
    assert!(w.store.marketplace(&id).unwrap().is_some());
}

#[test]
fn an_added_marketplace_cannot_claim_the_squrell_index_or_domain() {
    // The Squrell fixture served from another URL: it cannot be added as Squrell, nor under a
    // domain other than the one its index declares.
    let Some(w) = world("marketplace-squrell", &[SQ_TAG], COMMUNITY_URL, |_| {}) else {
        return;
    };
    let e = add_blocking(
        &w.store,
        &w.cfg,
        COMMUNITY_URL,
        "hub.squrell.cloud",
        "admin",
    )
    .unwrap_err();
    assert!(e.to_string().contains("Squrell"), "{e:?}");
    // It cannot be added under any other domain either: the index declares Squrell's.
    let e = add_blocking(
        &w.store,
        &w.cfg,
        COMMUNITY_URL,
        "market.copy.example",
        "admin",
    )
    .unwrap_err();
    assert!(e.to_string().contains("publisherDomain"), "{e:?}");
    assert_eq!(w.rows_of_marketplace_tier(), 0);
}

#[test]
fn tar_archives_are_deterministic_and_unpack_to_the_same_files() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("docs")).unwrap();
    std::fs::write(d.path().join("SKILLS.md"), "# s").unwrap();
    std::fs::write(d.path().join("docs/a.md"), "a").unwrap();
    let one = pack_tar_gz(d.path()).unwrap();
    let two = pack_tar_gz(d.path()).unwrap();
    assert_eq!(one, two);
    let out = tempfile::tempdir().unwrap();
    crate::skill_package_admit::unpack_tar_gz(&one, out.path()).unwrap();
    assert_eq!(
        std::fs::read_to_string(out.path().join("docs/a.md")).unwrap(),
        "a"
    );
    assert_eq!(
        vam_storage::skills_digest_from_dir(d.path()),
        vam_storage::skills_digest_from_dir(out.path())
    );
}

#[test]
fn refs_are_pinned_to_the_listing_or_refused() {
    let mut known = Pins::new();
    known.insert(
        "urn:vam:skill:d.example/a/s".into(),
        (release_version((1, 2, 3)), "d".repeat(64)),
    );
    let mk = |urn: &str, v: Option<(i32, i32, i32)>, digest: &str| ResourceRef {
        resource_id: Some(vam_spec::v1::ResourceId { urn: urn.into() }),
        version: v.map(release_version),
        digest: (!digest.is_empty()).then(|| Digest {
            algorithm: SHA256,
            hex_hash: digest.into(),
        }),
        locators: vec![],
    };
    // Floating, pinned right, and digest-less all end up frozen to the listing's release.
    for mut r in [
        mk("urn:vam:skill:d.example/a/s", None, ""),
        mk("urn:vam:skill:d.example/a/s", Some((1, 2, 3)), ""),
        mk(
            "urn:vam:skill:d.example/a/s",
            Some((1, 2, 3)),
            &"d".repeat(64),
        ),
    ] {
        pin_ref(&mut r, &known, "agent", "a skill").unwrap();
        assert_eq!(
            r.version.as_ref().map(|v| (v.major, v.minor, v.patch)),
            Some((1, 2, 3))
        );
        assert_eq!(r.digest.as_ref().unwrap().hex_hash, "d".repeat(64));
    }
    // A different version, a different digest, another artifact: refused.
    for mut r in [
        mk("urn:vam:skill:d.example/a/s", Some((2, 0, 0)), ""),
        mk(
            "urn:vam:skill:d.example/a/s",
            Some((1, 2, 3)),
            &"e".repeat(64),
        ),
        mk("urn:vam:skill:d.example/a/other", None, ""),
    ] {
        assert!(pin_ref(&mut r, &known, "agent", "a skill").is_err());
    }
    // An unset ref is an absent optional field.
    let mut empty = ResourceRef::default();
    pin_ref(&mut empty, &known, "agent", "x").unwrap();
    assert_eq!(empty, ResourceRef::default());
}

// ---- connectors in listings (CN-4.11) -----------------------------------------------------------

const CONNECTOR_URL: &str = "https://connectors.community.example/vam-marketplace";
const CONNECTOR_TAGS: [&str; 4] = [
    "bugwatch-connector-v1.0.0",
    "reserved-connector-id-v1.0.0",
    "unknown-profile-connector-v1.0.0",
    "connector-with-a-secret-v1.0.0",
];
const BUGWATCH: &str = "urn:vam:connector:connectors.community.example/community/bugwatch";
const BUGWATCH_FILE: &str = "listings/bugwatch-connector/connectors/bugwatch/connector.json";

fn connectors_world(edit: impl FnOnce(&mut TestRepo)) -> Option<(World, String)> {
    let w = world(
        "marketplace-community-connectors",
        &CONNECTOR_TAGS,
        CONNECTOR_URL,
        edit,
    )?;
    let row = add_blocking(
        &w.store,
        &w.cfg,
        CONNECTOR_URL,
        "connectors.community.example",
        "admin",
    )
    .unwrap();
    Some((w, row.id))
}

fn node_row(id: &str, enabled: bool) -> crate::connectors::NodeConnector {
    crate::connectors::NodeConnector {
        connector_id: id.into(),
        enabled,
        consent_tenant_id: None,
        consent_recorded_at: None,
        consent_recorded_by: None,
        updated_at: 1,
        updated_by: "olga".into(),
    }
}

fn connect_bugwatch(store: &SqliteWorkflowStore, user: &str) {
    connect_bugwatch_at(store, user, "https://mcp.bugwatch.example/mcp");
}

fn connect_bugwatch_at(store: &SqliteWorkflowStore, user: &str, url: &str) {
    let oauth = crate::mcp_registry::McpOauthMetadata {
        resource: url.into(),
        authorization_server: "https://auth.bugwatch.example".into(),
        token_endpoint: "https://auth.bugwatch.example/token".into(),
        revocation_endpoint: None,
        client_id: "c1".into(),
        registration_method: crate::mcp_registry::McpOauthRegistrationMethod::Dcr,
        scopes: vec!["read".into()],
        expires_at: None,
        status: crate::mcp_registry::McpOauthStatus::Connected,
        last_refresh_at: None,
        last_refresh_error: None,
    };
    store
        .upsert_mcp_connection(
            crate::secrets::SecretScope::User,
            user,
            "bugwatch",
            url,
            "",
            &[],
            &[],
            crate::mcp_registry::McpAuthMode::Oauth,
            Some(&oauth),
        )
        .unwrap();
}

fn status_of(store: &SqliteWorkflowStore, user: &str) -> crate::mcp_registry::McpOauthStatus {
    store
        .get_mcp_connection(crate::secrets::SecretScope::User, user, "bugwatch")
        .unwrap()
        .oauth
        .unwrap()
        .status
}

/// Commits a changed connector file and the index's new tag on the test repository, then reads
/// the marketplace and updates the listing to that tag.
fn release_connector(
    w: &mut World,
    id: &str,
    tag: &str,
    change: impl FnOnce(&mut serde_json::Value),
) -> Outcome {
    let work = w.repo.work_dir();
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(work.join(BUGWATCH_FILE)).unwrap()).unwrap();
    change(&mut v);
    let mut idx: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(work.join("marketplace.json")).unwrap())
            .unwrap();
    idx["listings"][0]["ref"] = tag.into();
    w.repo.write(&[
        (BUGWATCH_FILE, &serde_json::to_string_pretty(&v).unwrap()),
        (
            "marketplace.json",
            &serde_json::to_string_pretty(&idx).unwrap(),
        ),
    ]);
    w.repo.tag(tag);
    refresh_blocking(&w.store, &w.cfg, id).unwrap();
    install_blocking(
        &w.store,
        &w.cfg,
        id,
        "bugwatch-connector",
        Mode::Update,
        Some(tag),
        "admin",
    )
    .unwrap()
}

#[test]
fn a_marketplace_connector_installs_not_enabled_and_shows_where_it_came_from() {
    let Some((w, id)) = connectors_world(|_| {}) else {
        return;
    };
    // Before the install the connector is nowhere.
    assert!(w.store.connector_entry("bugwatch").unwrap().is_none());
    let out = w.install(&id, "bugwatch-connector", Mode::Install).unwrap();
    assert_eq!(out.artifacts.len(), 1);
    assert_eq!(out.artifacts[0].kind, "connector");
    assert!(!out.artifacts[0].summary_digest.is_empty());

    let e = w.store.connector_entry("bugwatch").unwrap().unwrap();
    let meta = e.installed.as_ref().unwrap();
    assert_eq!(e.trust_word(), "unverified");
    assert_eq!(meta.marketplace_id, id);
    assert_eq!(meta.listing, "bugwatch-connector");
    assert_eq!(meta.git_ref, "bugwatch-connector-v1.0.0");
    assert_eq!(meta.commit.len(), 40);
    assert_eq!(e.def.urn, BUGWATCH);
    // The built-ins are still first, and the id is a connector now.
    let ids: Vec<String> = w
        .store
        .connector_entries()
        .unwrap()
        .iter()
        .map(|e| e.id())
        .collect();
    assert_eq!(ids, ["linear", "microsoft-365", "bugwatch"]);

    // D4: nothing is enabled by the install, and a write for its id is refused until it is.
    assert!(!w.store.connector_enabled("bugwatch").unwrap());
    let refused = crate::connectors::gate_mcp_write(
        &w.store,
        "bugwatch",
        Some("https://mcp.bugwatch.example/mcp"),
        None,
    )
    .unwrap_err();
    assert!(matches!(refused, E::PermissionDenied(_)), "{refused:?}");
    // Its own URL only.
    let wrong = crate::connectors::gate_mcp_write(
        &w.store,
        "bugwatch",
        Some("https://mcp.evil.example/mcp"),
        None,
    )
    .unwrap_err();
    assert!(matches!(wrong, E::InvalidContract(_)), "{wrong:?}");
    w.store
        .put_node_connector(&node_row("bugwatch", true))
        .unwrap();
    let policy = crate::connectors::gate_mcp_write(
        &w.store,
        "bugwatch",
        Some("https://mcp.bugwatch.example/mcp"),
        None,
    )
    .unwrap()
    .unwrap();
    assert!(policy.is_vetted("list_issues") && !policy.is_vetted("export_workspace"));
    assert_eq!(policy.ask_tools, ["save_issue"]);

    // The origin is recorded like the other kinds', and the URN counts as installed (read-only).
    let c = w.store.connection.lock().unwrap();
    assert!(crate::catalog_tier::is_installed_urn(&c, BUGWATCH).unwrap());
    let n: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM catalog_origins WHERE kind = 'connector' AND urn = ?1",
            [BUGWATCH],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn a_connector_that_takes_a_squrell_id_names_an_unknown_profile_or_asks_for_a_secret_is_refused() {
    let Some((w, id)) = connectors_world(|_| {}) else {
        return;
    };
    for (listing, why) in [
        ("reserved-connector-id", "Squrell connector id"),
        ("unknown-profile-connector", "profile"),
        ("connector-with-a-secret", "secret"),
    ] {
        let e = w.install(&id, listing, Mode::Install).unwrap_err();
        assert!(e.to_string().contains(why), "{listing}: {e:?}");
    }
    assert_eq!(w.count("connector_releases", "%"), 0);
    assert!(w.store.connector_entry("oddjob").unwrap().is_none());
    // The built-in is untouched.
    assert!(w
        .store
        .connector_entry("linear")
        .unwrap()
        .unwrap()
        .installed
        .is_none());
}

#[test]
fn a_connector_urn_outside_the_marketplace_domain_is_refused() {
    let Some((w, id)) = connectors_world(|r| {
        let raw = std::fs::read_to_string(r.work_dir().join(BUGWATCH_FILE)).unwrap();
        r.write(&[(
            BUGWATCH_FILE,
            &raw.replace("connectors.community.example", "hub.squrell.cloud"),
        )]);
        r.tag("bugwatch-connector-v1.0.0");
    }) else {
        return;
    };
    let e = w
        .install(&id, "bugwatch-connector", Mode::Install)
        .unwrap_err();
    assert!(
        e.to_string()
            .contains("outside the marketplace's publisher domain"),
        "{e:?}"
    );
    assert_eq!(w.count("connector_releases", "%"), 0);
}

#[test]
fn a_listing_names_only_the_connector_it_ships() {
    let Some((w, id)) = connectors_world(|r| {
        let raw = std::fs::read_to_string(r.work_dir().join("marketplace.json")).unwrap();
        // The good listing names the built-in Linear connector instead of shipping its own.
        let at = "\"requiresNode\": \">=0.1.0\"";
        let raw = raw.replacen(
            at,
            &format!("{at}, \"connector\": \"urn:vam:connector:hub.squrell.cloud/squrell/linear\""),
            1,
        );
        r.write(&[("marketplace.json", &raw)]);
        r.tag("bugwatch-connector-v1.0.0");
    }) else {
        return;
    };
    let e = w
        .install(&id, "bugwatch-connector", Mode::Install)
        .unwrap_err();
    assert!(e.to_string().contains("does not ship it"), "{e:?}");
    assert_eq!(w.count("connector_releases", "%"), 0);
}

#[test]
fn an_installed_connector_id_cannot_be_taken_by_another_marketplace() {
    let Some((w, id)) = connectors_world(|_| {}) else {
        return;
    };
    w.install(&id, "bugwatch-connector", Mode::Install).unwrap();

    // A second marketplace, another domain, the same connector id.
    let mut other = TestRepo::new("marketplace-community-connectors", &CONNECTOR_TAGS);
    for p in ["marketplace.json", BUGWATCH_FILE] {
        let raw = std::fs::read_to_string(other.work_dir().join(p)).unwrap();
        other.write(&[(
            p,
            &raw.replace("connectors.community.example", "second.community.example"),
        )]);
    }
    for t in CONNECTOR_TAGS {
        other.tag(t);
    }
    let url2 = "https://second.community.example/vam-marketplace";
    fetch::TEST_SEAM.with(|m| {
        m.borrow_mut().insert(url2.to_string(), other.url());
    });
    let second = add_blocking(&w.store, &w.cfg, url2, "second.community.example", "admin").unwrap();
    let e = w
        .install(&second.id, "bugwatch-connector", Mode::Install)
        .unwrap_err();
    assert!(
        matches!(&e, E::Conflict(m) if m.contains("already taken")),
        "{e:?}"
    );
    assert_eq!(w.count("connector_releases", "%"), 1);
    // The first marketplace's connector is still the one.
    let e = w.store.connector_entry("bugwatch").unwrap().unwrap();
    assert_eq!(e.def.urn, BUGWATCH);
}

#[test]
fn an_update_that_changes_what_a_connector_can_do_needs_enable_again() {
    let Some((mut w, id)) = connectors_world(|_| {}) else {
        return;
    };
    w.install(&id, "bugwatch-connector", Mode::Install).unwrap();
    w.store
        .put_node_connector(&node_row("bugwatch", true))
        .unwrap();
    connect_bugwatch(&w.store, "ada");
    connect_bugwatch(&w.store, "bob");
    assert!(w.store.connector_enabled("bugwatch").unwrap());

    // Only wording changes: nothing to approve again.
    let out = release_connector(&mut w, &id, "bugwatch-connector-v1.0.1", |c| {
        c["description"] = "Bugwatch issues, reworded.".into();
        c["version"] = "1.0.1".into();
    });
    assert!(
        out.capabilities_changed.is_empty(),
        "{:?}",
        out.capabilities_changed
    );
    assert!(w.store.connector_enabled("bugwatch").unwrap());
    assert_eq!(
        status_of(&w.store, "ada"),
        crate::mcp_registry::McpOauthStatus::Connected
    );
    assert_eq!(
        w.store
            .connector_entry("bugwatch")
            .unwrap()
            .unwrap()
            .def
            .description,
        "Bugwatch issues, reworded."
    );

    // A scope is added: back to not enabled, members' connections need reauth, the old releases
    // stay.
    let out = release_connector(&mut w, &id, "bugwatch-connector-v1.1.0", |c| {
        c["provider"]["scopes"] = serde_json::json!(["read", "write", "admin"]);
        c["version"] = "1.1.0".into();
    });
    assert_eq!(out.capabilities_changed, [BUGWATCH]);
    assert!(!w.store.connector_enabled("bugwatch").unwrap());
    for u in ["ada", "bob"] {
        assert_eq!(
            status_of(&w.store, u),
            crate::mcp_registry::McpOauthStatus::NeedsReauth
        );
    }
    assert_eq!(w.versions("connector_releases", BUGWATCH).len(), 3);
    let e = w.store.connector_entry("bugwatch").unwrap().unwrap();
    assert_eq!(e.installed.unwrap().git_ref, "bugwatch-connector-v1.1.0");
}

#[test]
fn an_update_that_moves_the_server_leaves_a_stale_row_the_next_connect_replaces() {
    let Some((mut w, id)) = connectors_world(|_| {}) else {
        return;
    };
    w.install(&id, "bugwatch-connector", Mode::Install).unwrap();
    w.store
        .put_node_connector(&node_row("bugwatch", true))
        .unwrap();
    connect_bugwatch(&w.store, "ada");
    // A server a member added by hand under the id, on a URL the connector never named.
    connect_bugwatch_at(&w.store, "cy", "https://elsewhere.example/mcp");
    let out = release_connector(&mut w, &id, "bugwatch-connector-v2.0.0", |c| {
        c["mcp"]["url"] = "https://mcp2.bugwatch.example/mcp".into();
        c["version"] = "2.0.0".into();
    });
    assert_eq!(out.capabilities_changed, [BUGWATCH]);
    let conn = w
        .store
        .get_mcp_connection(crate::secrets::SecretScope::User, "ada", "bugwatch")
        .unwrap();
    // The row still names the old server and cannot run: it is marked, the connector is off.
    assert_eq!(conn.url, "https://mcp.bugwatch.example/mcp");
    let new_url = "https://mcp2.bugwatch.example/mcp";
    assert!(crate::connectors::is_stale_installed_row(&w.store, &conn, new_url).unwrap());
    // Only a connect to the connector's own URL replaces it.
    assert!(!crate::connectors::is_stale_installed_row(
        &w.store,
        &conn,
        "https://other.example/mcp"
    )
    .unwrap());
    // A row on a built-in connector's id is never "stale".
    let mut linear = conn.clone();
    linear.connection_id = "linear".into();
    assert!(!crate::connectors::is_stale_installed_row(&w.store, &linear, new_url).unwrap());
    // A server a member added by hand under the id, on a URL the connector never named, is not
    // marked by the update.
    assert_eq!(
        crate::mcp_registry::McpOauthStatus::NeedsReauth,
        status_of(&w.store, "ada")
    );
    assert_eq!(
        crate::mcp_registry::McpOauthStatus::Connected,
        status_of(&w.store, "cy")
    );
}

#[test]
fn a_new_install_never_inherits_an_enabled_flag_left_for_its_id() {
    let Some((w, id)) = connectors_world(|_| {}) else {
        return;
    };
    // A row left for the id (an earlier install that was reset): the install must not use it.
    w.store
        .put_node_connector(&node_row("bugwatch", true))
        .unwrap();
    w.install(&id, "bugwatch-connector", Mode::Install).unwrap();
    assert!(!w.store.connector_enabled("bugwatch").unwrap());
}
