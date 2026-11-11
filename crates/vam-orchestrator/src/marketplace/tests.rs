use super::testutil::{git_bin, TestRepo};
use super::*;

fn cfg(state: &Path) -> Option<MarketplaceConfig> {
    git_bin().map(|g| MarketplaceConfig {
        fetcher: Fetcher::new(Some(g), state.join("marketplaces"), Limits::default()),
        refresh_every: Some(Duration::from_secs(3600)),
        node_version: "0.1.0".into(),
    })
}

/// Makes the https URL stand for a local repository for this test thread.
fn map(url: &str, repo: &TestRepo) {
    fetch::TEST_SEAM.with(|m| {
        m.borrow_mut().insert(url.to_string(), repo.url());
    });
}

const SQ_URL: &str = "https://github.com/squrell-inc/vam-marketplace";
const COMMUNITY_URL: &str = "https://market.community.example/vam-marketplace";
const COMMUNITY_TAGS: [&str; 4] = [
    "helpful-notes-v1.0.0",
    "with-a-connector-v1.0.0",
    "claims-the-squrell-name-v1.0.0",
    "claims-the-org-name-v1.0.0",
];

fn input<'a>(
    id: &'a str,
    r: &TestRepo,
    domain: &'a str,
    pins: &'a BTreeMap<String, String>,
) -> ReadInput<'a> {
    ReadInput {
        id,
        remote: Remote::File { url: r.url() },
        confirmed_domain: domain,
        pins,
        node_version: "0.1.0",
    }
}

fn listing<'a>(d: &'a RefreshData, id: &str) -> &'a ListingState {
    d.listings.iter().find(|l| l.id == id).unwrap()
}

// ---- reading ----------------------------------------------------------------------------------

#[test]
fn the_squrell_listing_is_read_at_its_tag_with_what_it_installs_and_can_reach() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let r = TestRepo::new("marketplace-squrell", &["microsoft-365-v1.0.0"]);
    let pins = BTreeMap::new();
    let read = read_marketplace(
        &c.fetcher,
        &input("squrell", &r, "hub.squrell.cloud", &pins),
    )
    .unwrap();
    assert!(read.moved.is_empty());
    let d = &read.data;
    assert_eq!(d.name, "Squrell");
    assert_eq!(d.head_commit, r.head());
    assert_eq!(d.index_digest.len(), 64);
    let l = listing(d, "microsoft-365");
    assert_eq!(l.error, None, "{l:?}");
    assert_eq!(l.git_ref, "microsoft-365-v1.0.0");
    assert_eq!(
        l.commit.as_deref(),
        Some(r.rev("microsoft-365-v1.0.0").as_str())
    );
    assert_eq!(
        l.connector.as_deref(),
        Some("urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365")
    );
    let kinds: Vec<&str> = l.artifacts.iter().map(|a| a.kind.as_str()).collect();
    assert_eq!(kinds, ["skill", "playbook", "agent"]);
    assert!(
        l.artifacts
            .iter()
            .all(|a| a.urn.contains(":hub.squrell.cloud/")),
        "{:?}",
        l.artifacts
    );
    let s = l.summary.as_ref().unwrap();
    assert_eq!(s["artifacts"].as_array().unwrap().len(), 3);
    assert!(!s["tools"].as_array().unwrap().is_empty());
    // The tag is pinned the first time it is read, and the tags the repo has are listed.
    assert_eq!(
        d.new_pins,
        vec![(
            "microsoft-365-v1.0.0".to_string(),
            r.rev("microsoft-365-v1.0.0")
        )]
    );
    assert_eq!(d.tags_seen.len(), 1);
}

#[test]
fn a_community_listing_names_a_whole_host_prefix_as_any_path() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let r = TestRepo::new("marketplace-community", &COMMUNITY_TAGS);
    let pins = BTreeMap::new();
    let read = read_marketplace(
        &c.fetcher,
        &input("m-1", &r, "market.community.example", &pins),
    )
    .unwrap();
    let good = listing(&read.data, "helpful-notes");
    assert_eq!(good.error, None, "{good:?}");
    let e = &good.summary.as_ref().unwrap()["egress"][0];
    assert_eq!(e["host"], "notes.example.test");
    assert_eq!(e["anyPath"], true);
    assert_eq!(e["paths"][0]["anyPath"], true);
    // A listing that ships a connector shows it (the node refuses it at install, CN-4.5).
    let with = listing(&read.data, "with-a-connector");
    assert_eq!(
        with.connector.as_deref(),
        Some("urn:vam:connector:market.community.example/community/notes")
    );
    // Artifacts outside the marketplace's own domain are flagged on the listing.
    let sq = listing(&read.data, "claims-the-squrell-name");
    assert!(
        sq.error.as_deref().unwrap().contains("hub.squrell.cloud"),
        "{sq:?}"
    );
    let org = listing(&read.data, "claims-the-org-name");
    assert!(
        org.error.as_deref().unwrap().contains("local.vam.dev"),
        "{org:?}"
    );
    assert!(sq.summary.is_none() && org.summary.is_none());
    // One listing's problem leaves the others readable.
    assert!(good.summary.is_some());
}

#[test]
fn an_index_that_declares_another_publisher_domain_is_refused() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let r = TestRepo::new("marketplace-community", &COMMUNITY_TAGS);
    let pins = BTreeMap::new();
    let err = read_marketplace(&c.fetcher, &input("m-1", &r, "other.example.com", &pins))
        .err()
        .unwrap();
    assert!(
        matches!(&err, ReadError::DomainChanged { declared, .. } if declared == "market.community.example"),
        "{err}"
    );
}

#[test]
fn a_broken_index_is_an_error_not_a_panic() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let mut r = TestRepo::new("marketplace-community", &COMMUNITY_TAGS);
    r.overlay("marketplace-community-broken");
    let pins = BTreeMap::new();
    let err = read_marketplace(
        &c.fetcher,
        &input("m-1", &r, "market.community.example", &pins),
    )
    .err()
    .unwrap();
    assert!(matches!(err, ReadError::Index(_)), "{err}");
    // Oversize, a symlinked index and a missing one fail the same way.
    r.write(&[("marketplace.json", &" ".repeat(300 * 1024))]);
    assert!(matches!(
        read_marketplace(
            &c.fetcher,
            &input("m-1", &r, "market.community.example", &pins)
        )
        .err()
        .unwrap(),
        ReadError::Index(_)
    ));
}

#[test]
fn a_listing_whose_tag_is_missing_says_so_and_does_not_hide_the_others() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let r = TestRepo::new("marketplace-community", &COMMUNITY_TAGS[..3]);
    let pins = BTreeMap::new();
    let read = read_marketplace(
        &c.fetcher,
        &input("m-1", &r, "market.community.example", &pins),
    )
    .unwrap();
    let org = listing(&read.data, "claims-the-org-name");
    assert!(
        org.error.as_deref().unwrap().contains("does not exist"),
        "{org:?}"
    );
    assert!(listing(&read.data, "helpful-notes").summary.is_some());
}

#[test]
fn a_node_too_old_for_a_listing_is_told_so() {
    let state = tempfile::tempdir().unwrap();
    let Some(mut c) = cfg(state.path()) else {
        return;
    };
    c.node_version = "0.0.1".into();
    let r = TestRepo::new("marketplace-squrell", &["microsoft-365-v1.0.0"]);
    let pins = BTreeMap::new();
    let mut i = input("squrell", &r, "hub.squrell.cloud", &pins);
    i.node_version = "0.0.1";
    let read = read_marketplace(&c.fetcher, &i).unwrap();
    assert!(listing(&read.data, "microsoft-365")
        .error
        .as_deref()
        .unwrap()
        .contains("requires node"));
}

// ---- tags: pinned, moved, newer ---------------------------------------------------------------

#[test]
fn a_moved_tag_is_refused_and_the_pin_keeps_the_first_commit() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let mut r = TestRepo::new("marketplace-squrell", &["microsoft-365-v1.0.0"]);
    map(SQ_URL, &r);
    let first = refresh_blocking(&store, &c, "squrell").unwrap();
    assert!(
        first.refused.is_none() && first.row.last_error.is_none(),
        "{:?}",
        first.row.last_error
    );
    let pinned = r.rev("microsoft-365-v1.0.0");
    assert_eq!(
        store
            .marketplace_pin("squrell", "microsoft-365-v1.0.0")
            .unwrap(),
        Some(pinned.clone())
    );
    // The tag is re-pointed at a different commit.
    r.overlay("marketplace-squrell-moved-tag");
    r.tag("microsoft-365-v1.0.0");
    assert_ne!(r.rev("microsoft-365-v1.0.0"), pinned);
    let second = refresh_blocking(&store, &c, "squrell").unwrap();
    let why = second.refused.expect("refused");
    assert!(
        why.contains("moved") && why.contains("microsoft-365-v1.0.0"),
        "{why}"
    );
    assert_eq!(second.row.last_error.as_deref(), Some(why.as_str()));
    let l = &second.row.listings[0];
    assert!(l.error.as_deref().unwrap().contains("moved"), "{l:?}");
    assert_eq!(
        l.commit.as_deref(),
        Some(pinned.as_str()),
        "the listing stays at the pinned commit"
    );
    assert_eq!(
        store
            .marketplace_pin("squrell", "microsoft-365-v1.0.0")
            .unwrap(),
        Some(pinned.clone()),
        "never followed"
    );
    // CN-4.5's re-check at install time refuses it too.
    let err = cached_tag_commit(&store, &c, &second.row, "microsoft-365-v1.0.0").unwrap_err();
    assert!(
        matches!(err, CachedTagError::Moved(_)) && err.to_string().contains("moved"),
        "{err}"
    );
    // Putting the tag back clears the refusal on the next read.
    r.git(&["tag", "-f", "microsoft-365-v1.0.0", &pinned]);
    r.publish();
    let third = refresh_blocking(&store, &c, "squrell").unwrap();
    assert!(third.refused.is_none() && third.row.last_error.is_none());
}

#[test]
fn a_newer_tag_is_pinned_beside_the_old_one() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let mut r = TestRepo::new("marketplace-squrell", &["microsoft-365-v1.0.0"]);
    map(SQ_URL, &r);
    refresh_blocking(&store, &c, "squrell").unwrap();
    r.overlay("marketplace-squrell-update");
    r.tag("microsoft-365-v1.1.0");
    let done = refresh_blocking(&store, &c, "squrell").unwrap();
    assert!(done.refused.is_none());
    let l = &done.row.listings[0];
    assert_eq!(l.git_ref, "microsoft-365-v1.1.0");
    assert_eq!(
        l.commit.as_deref(),
        Some(r.rev("microsoft-365-v1.1.0").as_str())
    );
    let pins = store.marketplace_pins("squrell").unwrap();
    assert_eq!(pins.len(), 2);
    assert_eq!(pins["microsoft-365-v1.0.0"], r.rev("microsoft-365-v1.0.0"));
    assert_eq!(done.row.tags_seen.len(), 2);
    // Both commits stay readable from the cache for CN-4.5.
    let old = cached_tag_commit(&store, &c, &done.row, "microsoft-365-v1.0.0").unwrap();
    assert_eq!(old, r.rev("microsoft-365-v1.0.0"));
    let new = cached_tag_commit(&store, &c, &done.row, "microsoft-365-v1.1.0").unwrap();
    assert_eq!(new, r.rev("microsoft-365-v1.1.0"));
    let out = tempfile::tempdir().unwrap();
    c.fetcher
        .export_dir(
            "squrell",
            &old,
            "listings/microsoft-365/agents/executive-assistant",
            out.path(),
        )
        .unwrap();
    assert!(out.path().join("agent.json").is_file());
    // A tag that was never read is not installable.
    assert!(matches!(
        cached_tag_commit(&store, &c, &done.row, "nope-v1"),
        Err(CachedTagError::Unpinned(_))
    ));
}

// ---- refresh: one marketplace at a time -------------------------------------------------------

#[test]
fn a_broken_index_sets_that_marketplaces_error_and_keeps_its_last_good_listings() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let sq = TestRepo::new("marketplace-squrell", &["microsoft-365-v1.0.0"]);
    let mut community = TestRepo::new("marketplace-community", &COMMUNITY_TAGS);
    map(SQ_URL, &sq);
    map(COMMUNITY_URL, &community);
    let added = add_blocking(&store, &c, COMMUNITY_URL, "market.community.example", "u1").unwrap();
    assert_eq!(added.trust, store::Trust::Unverified);
    assert_eq!(added.name, "Community");
    assert!(!added.preconfigured && added.enabled && added.last_error.is_none());
    assert_eq!(added.listings.len(), 4);

    community.overlay("marketplace-community-broken");
    let broken = refresh_blocking(&store, &c, &added.id).unwrap();
    assert!(
        broken.refused.is_none(),
        "a broken index is a 200 with lastError"
    );
    assert!(
        broken.row.last_error.as_deref().unwrap().contains("index"),
        "{:?}",
        broken.row.last_error
    );
    assert_eq!(broken.row.listings.len(), 4, "the last good listings stay");
    // The other marketplace refreshes cleanly.
    let ok = refresh_blocking(&store, &c, "squrell").unwrap();
    assert!(ok.row.last_error.is_none() && ok.refused.is_none());
    assert_eq!(ok.row.listings.len(), 1);
    // Fixing the index clears the error.
    community.git(&["revert", "--no-edit", "HEAD"]);
    community.publish();
    let fixed = refresh_blocking(&store, &c, &added.id).unwrap();
    assert!(fixed.row.last_error.is_none(), "{:?}", fixed.row.last_error);
}

#[test]
fn a_changed_publisher_domain_is_refused_and_recorded() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let mut r = TestRepo::new("marketplace-community", &COMMUNITY_TAGS);
    map(COMMUNITY_URL, &r);
    let added = add_blocking(&store, &c, COMMUNITY_URL, "market.community.example", "u1").unwrap();
    let idx = std::fs::read_to_string(r.work_dir().join("marketplace.json")).unwrap();
    r.write(&[(
        "marketplace.json",
        &idx.replace("market.community.example", "hub.squrell.cloud"),
    )]);
    let done = refresh_blocking(&store, &c, &added.id).unwrap();
    let why = done.refused.expect("refused");
    assert!(
        why.contains("hub.squrell.cloud") && why.contains("market.community.example"),
        "{why}"
    );
    assert_eq!(
        store
            .marketplace(&added.id)
            .unwrap()
            .unwrap()
            .publisher_domain,
        "market.community.example",
        "the confirmed domain never changes"
    );
}

#[test]
fn a_failing_remote_is_that_marketplaces_error_only() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let store = SqliteWorkflowStore::in_memory().unwrap();
    // The Squrell URL maps to a repository that does not exist.
    fetch::TEST_SEAM.with(|m| {
        m.borrow_mut()
            .insert(SQ_URL.into(), "file:///nonexistent/bare.git".into());
    });
    let done = refresh_blocking(&store, &c, "squrell").unwrap();
    assert!(done.refused.is_none());
    let err = done.row.last_error.unwrap();
    assert!(err.contains("git failed"), "{err}");
    assert!(
        !err.contains(&state.path().display().to_string()),
        "state paths are scrubbed: {err}"
    );
    assert!(done.row.last_checked_at.is_some());
}

#[test]
fn a_disabled_marketplace_is_not_read_and_shows_no_listings() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let r = TestRepo::new("marketplace-squrell", &["microsoft-365-v1.0.0"]);
    map(SQ_URL, &r);
    refresh_blocking(&store, &c, "squrell").unwrap();
    store.set_marketplace_enabled("squrell", false).unwrap();
    let err = refresh_blocking(&store, &c, "squrell").err().unwrap();
    assert!(matches!(err, OrchestratorError::Conflict(_)));
    let row = store.marketplace("squrell").unwrap().unwrap();
    assert_eq!(listings_view(&row), json!([]));
    assert!(!is_due(&row, i64::MAX / 2, Duration::from_secs(1)));
    assert!(matches!(
        refresh_blocking(&store, &c, "m-nope").err().unwrap(),
        OrchestratorError::NotFound(_)
    ));
}

// ---- adding -----------------------------------------------------------------------------------

#[test]
fn adding_refuses_bad_urls_before_any_network() {
    let state = tempfile::tempdir().unwrap();
    // No git at all: every refusal here happens before git would start.
    let c = MarketplaceConfig {
        fetcher: Fetcher::new(None, state.path().join("m"), Limits::default()),
        refresh_every: None,
        node_version: "0.1.0".into(),
    };
    let store = SqliteWorkflowStore::in_memory().unwrap();
    for url in [
        "http://git.example.com/a/b",
        "ssh://git@git.example.com/a/b",
        "file:///srv/repo",
        "https://127.0.0.1/a/b",
        "https://localhost/a/b",
        "https://user:pw@git.example.com/a/b",
        "https://git.example.com:8443/a/b",
        "https://169.254.169.254/latest",
        "not a url",
    ] {
        let err = add_blocking(&store, &c, url, "git.example.com", "u").unwrap_err();
        assert!(
            matches!(err, OrchestratorError::InvalidContract(_)),
            "{url}: {err}"
        );
    }
    assert!(
        !state.path().join("m").join(".home").exists(),
        "git never started"
    );
    let err = add_blocking(&store, &c, "https://git.example.com/a/b", "", "u").unwrap_err();
    assert!(
        err.to_string().contains("publisherDomain is required"),
        "{err}"
    );
    let err = add_blocking(
        &store,
        &c,
        "https://git.example.com/a/b",
        "Not A Domain",
        "u",
    )
    .unwrap_err();
    assert!(matches!(err, OrchestratorError::InvalidContract(_)));
    assert_eq!(store.marketplaces().unwrap().len(), 1, "nothing was added");
}

#[test]
fn an_added_marketplace_cannot_claim_a_reserved_domain() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let r = TestRepo::new("marketplace-community", &COMMUNITY_TAGS);
    map(COMMUNITY_URL, &r);
    for domain in [
        "hub.squrell.cloud",
        "eu.hub.squrell.cloud",
        "local.vam.dev",
        "vam.local",
        "examples.local",
        "team.dev.local",
    ] {
        let err = add_blocking(&store, &c, COMMUNITY_URL, domain, "u").unwrap_err();
        assert!(
            matches!(err, OrchestratorError::InvalidContract(_)),
            "{domain}: {err}"
        );
    }
    assert_eq!(store.marketplaces().unwrap().len(), 1);
}

#[test]
fn reserved_domains_include_the_orgs_own_and_other_marketplaces() {
    let org = RegistryDomains::new("vam.squrell.dev", Some("hub.staging.example")).unwrap();
    assert!(reserved_domain_reason("hub.squrell.cloud", &org, &[]).is_some());
    assert!(reserved_domain_reason("hub.staging.example", &org, &[]).is_some());
    assert!(reserved_domain_reason("vam.squrell.dev", &org, &[])
        .unwrap()
        .contains("registry domain"));
    assert!(reserved_domain_reason("local.vam.dev", &org, &[]).is_some());
    assert!(reserved_domain_reason("market.community.example", &org, &[]).is_none());
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let sq = store.marketplace("squrell").unwrap().unwrap();
    // The Squrell row's own domain is `hub.squrell.cloud`; another marketplace's domain is taken.
    let mut other = sq.clone();
    other.publisher_domain = "market.community.example".into();
    other.name = "Community".into();
    let why = reserved_domain_reason("market.community.example", &org, &[&other]).unwrap();
    assert!(why.contains("Community"), "{why}");
}

#[test]
fn adding_twice_or_adding_squrells_url_again_is_a_conflict() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let r = TestRepo::new("marketplace-community", &COMMUNITY_TAGS);
    map(COMMUNITY_URL, &r);
    add_blocking(&store, &c, COMMUNITY_URL, "market.community.example", "u").unwrap();
    for (url, domain) in [
        (COMMUNITY_URL, "market.community.example"),
        (
            "https://market.community.example/vam-marketplace.git",
            "other.example.com",
        ),
        (
            "https://MARKET.community.example/Vam-Marketplace/",
            "other.example.com",
        ),
        // The preconfigured URL cannot be added again to gain its trust.
        (
            "https://github.com/squrell-inc/vam-marketplace.git",
            "other.example.com",
        ),
        (
            "https://github.com/Squrell-Inc/vam-marketplace",
            "other.example.com",
        ),
    ] {
        let err = add_blocking(&store, &c, url, domain, "u").unwrap_err();
        assert!(
            matches!(err, OrchestratorError::Conflict(_)),
            "{url}: {err}"
        );
    }
    // The same publisher domain through a different URL is refused as well.
    let err = add_blocking(
        &store,
        &c,
        "https://git.example.com/a/b",
        "market.community.example",
        "u",
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("already the publisher domain"),
        "{err}"
    );
    assert!(
        store
            .marketplaces()
            .unwrap()
            .iter()
            .filter(|m| m.trust == store::Trust::Squrell)
            .count()
            == 1
    );
}

#[test]
fn adding_refuses_a_domain_the_declared_index_does_not_match_and_leaves_no_row_or_cache() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let r = TestRepo::new("marketplace-community", &COMMUNITY_TAGS);
    map(COMMUNITY_URL, &r);
    let err = add_blocking(&store, &c, COMMUNITY_URL, "something.else.example", "u").unwrap_err();
    assert!(
        err.to_string().contains("market.community.example"),
        "{err}"
    );
    assert_eq!(store.marketplaces().unwrap().len(), 1);
    let leftovers: Vec<_> = std::fs::read_dir(state.path().join("marketplaces"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn an_index_named_squrell_cannot_be_added() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let mut r = TestRepo::new("marketplace-community", &COMMUNITY_TAGS);
    let idx = std::fs::read_to_string(r.work_dir().join("marketplace.json")).unwrap();
    r.write(&[(
        "marketplace.json",
        &idx.replace("\"Community\"", "\"S.qurell\""),
    )]);
    map(COMMUNITY_URL, &r);
    let err = add_blocking(&store, &c, COMMUNITY_URL, "market.community.example", "u").unwrap_err();
    assert!(err.to_string().contains("imitates"), "{err}");
}

#[test]
fn removing_an_added_marketplace_drops_its_cache_and_the_preconfigured_one_stays() {
    let state = tempfile::tempdir().unwrap();
    let Some(c) = cfg(state.path()) else { return };
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let r = TestRepo::new("marketplace-community", &COMMUNITY_TAGS);
    map(COMMUNITY_URL, &r);
    let added = add_blocking(&store, &c, COMMUNITY_URL, "market.community.example", "u").unwrap();
    let cache = c.fetcher.cache_dir(&added.id).unwrap();
    assert!(cache.is_dir());
    delete_blocking(&store, &c, &added.id).unwrap();
    assert!(!cache.exists() && store.marketplace(&added.id).unwrap().is_none());
    assert!(store.marketplace_pins(&added.id).unwrap().is_empty());
    assert!(matches!(
        delete_blocking(&store, &c, "squrell").unwrap_err(),
        OrchestratorError::Conflict(_)
    ));
    assert!(matches!(
        delete_blocking(&store, &c, &added.id).unwrap_err(),
        OrchestratorError::NotFound(_)
    ));
}

// ---- views, timer, configuration --------------------------------------------------------------

#[test]
fn the_marketplace_view_omits_what_is_unset() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let mut row = store.marketplace("squrell").unwrap().unwrap();
    let v = marketplace_view(&row);
    assert_eq!(v["trust"], "squrell");
    assert_eq!(v["preconfigured"], true);
    assert_eq!(v["enabled"], true);
    assert_eq!(v["name"], "Squrell");
    for absent in ["lastError", "lastCheckedAt", "indexDigest"] {
        assert!(
            v.get(absent).is_none(),
            "{absent} must be absent when unset"
        );
    }
    row.last_error = Some("boom".into());
    row.last_checked_at = Some(7);
    let v = marketplace_view(&row);
    assert_eq!(
        (v["lastError"].as_str(), v["lastCheckedAt"].as_i64()),
        (Some("boom"), Some(7))
    );
}

#[test]
fn a_listing_shows_nothing_installed_and_no_update() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let mut row = store.marketplace("squrell").unwrap().unwrap();
    row.listings = vec![ListingState {
        id: "microsoft-365".into(),
        display_name: "Microsoft 365".into(),
        description: "d".into(),
        connector: Some("urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365".into()),
        git_ref: "microsoft-365-v1.0.0".into(),
        requires_node: ">=0.1.0".into(),
        commit: Some("a".repeat(40)),
        artifacts: vec![],
        summary: Some(json!({ "tools": [] })),
        error: None,
    }];
    let v = listings_view(&row);
    let l = &v[0];
    assert_eq!(l["ref"], "microsoft-365-v1.0.0");
    assert!(l["installed"].is_null() && l.get("installed").is_some());
    assert_eq!(l["updateAvailable"], false);
    assert_eq!(
        l["connector"],
        "urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365"
    );
    assert!(l.get("error").is_none());
}

#[test]
fn each_marketplace_is_due_on_its_own_clock() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let mut row = store.marketplace("squrell").unwrap().unwrap();
    let every = Duration::from_secs(6 * 3600);
    assert!(is_due(&row, 1000, every), "never read: due");
    row.last_checked_at = Some(1000);
    assert!(!is_due(&row, 1000 + 6 * 3600 - 1, every));
    assert!(is_due(&row, 1000 + 6 * 3600, every));
    row.enabled = false;
    assert!(!is_due(&row, 1000 + 7 * 3600, every));
}

#[test]
fn git_is_found_by_path_or_by_the_configured_binary() {
    assert_eq!(
        resolve_git_bin(Some("git")),
        None,
        "a relative path is refused"
    );
    assert_eq!(resolve_git_bin(Some("/nonexistent/git")), None);
    assert_eq!(resolve_git_bin(Some("/etc/passwd")), None, "not executable");
    if let Some(g) = resolve_git_bin(None) {
        assert!(g.is_absolute());
        assert_eq!(resolve_git_bin(Some(&g.display().to_string())), Some(g));
    }
}

#[test]
fn only_the_preconfigured_marketplace_is_squrell_trust() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let rows = store.marketplaces().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].url, SQURELL_URL);
    assert_eq!(rows[0].trust, store::Trust::Squrell);
    assert_eq!(rows[0].publisher_domain, SQURELL_DOMAIN);
}
