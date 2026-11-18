//! The rules for an agent saved by a caller who holds `agents:draft` and not
//! `catalog:write` (restricted mode; locked decisions 6 to 9 of the
//! `member-draft-agents` workstream).
//!
//! A restricted save writes the caller's own draft row and nothing else. It never
//! releases. The pieces here are pure checks; [`SqliteWorkflowStore::publish_agent_draft_restricted`]
//! (in `catalog_publish`) runs them in order and holds the store lock for the namespace,
//! owner and cap checks and the write.
//!
//! [`SqliteWorkflowStore::publish_agent_draft_restricted`]: crate::workflow::SqliteWorkflowStore

use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::{Connection, OptionalExtension};
use vam_spec::v1::agent_spec::InstructionSource;
use vam_spec::v1::{AgentSpec, ResourceRef};

use crate::catalog_resolve::{not_released, resolve_ref_for_owner, ArtifactKind, ResolveMode};
use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

/// Agent drafts one member may own when `VAMD_MEMBER_DRAFT_AGENTS_MAX` is unset.
pub const DEFAULT_MEMBER_DRAFT_AGENTS_MAX: usize = 50;

pub const ENV_MEMBER_DRAFT_AGENTS_MAX: &str = "VAMD_MEMBER_DRAFT_AGENTS_MAX";

static MEMBER_DRAFT_AGENTS_MAX: AtomicUsize = AtomicUsize::new(DEFAULT_MEMBER_DRAFT_AGENTS_MAX);

/// A positive whole number, else the default (a typo must not remove the cap).
pub fn parse_member_draft_agents_max(raw: Option<&str>) -> usize {
    raw.map(str::trim)
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MEMBER_DRAFT_AGENTS_MAX)
}

/// Install the per-member draft agent cap, read once from the environment at start.
pub fn install_member_draft_agents_max(max: usize) {
    MEMBER_DRAFT_AGENTS_MAX.store(max, Ordering::SeqCst);
}

/// The installed cap.
pub fn member_draft_agents_max() -> usize {
    MEMBER_DRAFT_AGENTS_MAX.load(Ordering::SeqCst)
}

/// Decision 8: fields a restricted save refuses. The first one found is named in a 400.
pub(crate) fn refused_field(spec: &AgentSpec) -> Option<&'static str> {
    if spec.capability_policy.is_some() {
        return Some("capabilityPolicy");
    }
    if spec.hitl_policy.is_some() {
        return Some("hitlPolicy");
    }
    if matches!(
        spec.instruction_source,
        Some(InstructionSource::InstructionArtifact(_))
    ) {
        return Some("instructionArtifact");
    }
    if spec.extends.is_some() {
        return Some("extends");
    }
    if spec.overlay.is_some() {
        return Some("overlay");
    }
    if spec
        .completion_policy
        .as_ref()
        .is_some_and(|c| !c.evaluator_refs.is_empty())
    {
        return Some("completionPolicy.evaluatorRefs");
    }
    None
}

/// Every skill, playbook and policy ref a saved agent carries, with its kind. Optional
/// single refs with no `resource_id` are unset fields and are left out.
fn refs_to_check(spec: &AgentSpec) -> Vec<(ArtifactKind, &ResourceRef)> {
    let mut refs: Vec<(ArtifactKind, &ResourceRef)> = Vec::new();
    refs.extend(spec.skill_refs.iter().map(|r| (ArtifactKind::Skill, r)));
    let set = |r: &&ResourceRef| r.resource_id.is_some();
    refs.extend(
        spec.default_playbook_ref
            .iter()
            .chain(spec.allowed_playbook_refs.iter())
            .filter(set)
            .map(|r| (ArtifactKind::Playbook, r)),
    );
    refs.extend(
        spec.permissions
            .iter()
            .flat_map(|p| p.advanced.iter())
            .filter(set)
            .map(|r| (ArtifactKind::Policy, r)),
    );
    refs.extend(
        spec.telemetry_policy
            .iter()
            .filter_map(|t| t.policy_ref.as_ref())
            .filter(set)
            .map(|r| (ArtifactKind::Policy, r)),
    );
    refs.extend(
        spec.completion_policy
            .iter()
            .filter_map(|c| c.policy_ref.as_ref())
            .filter(set)
            .map(|r| (ArtifactKind::Policy, r)),
    );
    refs
}

/// Decision 7 at save: every ref resolves to an existing release
/// ([`ResolveMode::ReleasedOnly`], the resolver the run uses).
pub(crate) fn require_released_refs(
    store: &SqliteWorkflowStore,
    spec: &AgentSpec,
    owner: &str,
) -> Result<(), OrchestratorError> {
    for (kind, resource_ref) in refs_to_check(spec) {
        match resolve_ref_for_owner(store, kind, resource_ref, owner, ResolveMode::ReleasedOnly) {
            Ok(_) => {}
            Err(OrchestratorError::NotFound(_) | OrchestratorError::InvalidContract(_)) => {
                let urn = resource_ref
                    .resource_id
                    .as_ref()
                    .map(|id| id.urn.as_str())
                    .unwrap_or("");
                return Err(not_released(kind, urn));
            }
            Err(other) => return Err(other),
        }
    }
    Ok(())
}

/// Decision 8 as an error: names the field so the author knows what to remove. Runs first,
/// before `validate`, so the message is this one and not a shape error about the same field.
pub(crate) fn require_no_refused_fields(spec: &AgentSpec) -> Result<(), OrchestratorError> {
    match refused_field(spec) {
        Some(field) => Err(OrchestratorError::InvalidContract(format!(
            "agent field '{field}' is not available to an agent saved by a member; remove it"
        ))),
        None => Ok(()),
    }
}

/// Decision 9: a new agent URN is under `urn:vam:agent:<registry domain>/<namespace>/`. The
/// namespace is `default_namespace(username)`, the value `GET /v1/me` reports. A user row
/// that is missing reads the user id as the username, as `/v1/me` does. Runs on the locked
/// connection.
pub(crate) fn require_own_namespace(
    connection: &Connection,
    owner: &str,
    urn: &str,
) -> Result<(), OrchestratorError> {
    let username: String = connection
        .query_row(
            "SELECT username FROM users WHERE user_id = ?1",
            [owner],
            |row| row.get(0),
        )
        .optional()
        .map_err(OrchestratorError::Database)?
        .unwrap_or_else(|| owner.to_string());
    let namespace = vam_spec::default_namespace(&username);
    let domains = crate::registry::registry_domains();
    let prefix = format!("urn:vam:agent:{}/{namespace}/", domains.registry_domain());
    if urn.len() > prefix.len() && urn.starts_with(&prefix) {
        Ok(())
    } else {
        Err(OrchestratorError::InvalidContract(format!(
            "a new agent saved by a member must be under {prefix} (your namespace); got '{urn}'"
        )))
    }
}

/// Decision 9: at most `max` agent drafts per member. Counts draft rows only, with an unset
/// or empty owner read as `local`. Runs on the locked connection, in the same critical
/// section as the insert it guards.
pub(crate) fn require_draft_capacity(
    connection: &Connection,
    owner: &str,
    max: usize,
) -> Result<(), OrchestratorError> {
    let owned: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM agents
             WHERE channel = 'draft'
               AND COALESCE(NULLIF(owner_user_id, ''), ?2) = ?1",
            rusqlite::params![owner, crate::users::LOCAL_USER_ID],
            |row| row.get(0),
        )
        .map_err(OrchestratorError::Database)?;
    if usize::try_from(owned).unwrap_or(usize::MAX) >= max {
        return Err(OrchestratorError::Conflict(format!(
            "a member may keep at most {max} draft agents; edit one of your drafts"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vam_spec::v1::{
        CapabilityPolicyRef, CompletionPolicyRef, Digest, HitlPolicyRef, PlaybookActivation,
        PlaybookSpec, PlaybookStep, ResourceId, SkillChannel, SkillManifest, SkillPackageRecord,
        StepMode, TelemetryPolicyRef, Version,
    };

    const SKILL: &str = "urn:vam:skill:local.vam.dev/acme/released-skill";
    const DRAFT_SKILL: &str = "urn:vam:skill:local.vam.dev/acme/draft-skill";
    const PLAYBOOK: &str = "urn:vam:playbook:local.vam.dev/acme/released-pb";
    const POLICY: &str = "urn:vam:policy:local.vam.dev/acme/released-policy";
    const DRAFT_POLICY: &str = "urn:vam:policy:local.vam.dev/acme/draft-policy";
    const CEDAR: &str = "permit(principal, action, resource);";
    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    type SpecEdit = Box<dyn Fn(&mut AgentSpec)>;

    fn rref(urn: &str) -> ResourceRef {
        ResourceRef {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            ..Default::default()
        }
    }

    fn versioned(urn: &str, prerelease: &str) -> ResourceRef {
        let mut r = rref(urn);
        r.version = Some(Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: prerelease.to_string(),
            build: String::new(),
        });
        r
    }

    fn package(urn: &str, prerelease: &str, channel: SkillChannel) -> SkillPackageRecord {
        let version = Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: prerelease.to_string(),
            build: String::new(),
        };
        SkillPackageRecord {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            version: Some(version.clone()),
            channel: channel as i32,
            digest: DIGEST.to_string(),
            blob_relpath: format!("local/sha256-{DIGEST}.tar.gz"),
            size_bytes: 8,
            spec: Some(SkillManifest {
                resource_id: Some(ResourceId {
                    urn: urn.to_string(),
                }),
                version: Some(version),
                description: "Test".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn playbook() -> PlaybookSpec {
        PlaybookSpec {
            activation: Some(PlaybookActivation::asking("Use when asked.")),
            resource_id: Some(ResourceId {
                urn: PLAYBOOK.to_string(),
            }),
            display_name: "pb".to_string(),
            description: "test".to_string(),
            steps: vec![PlaybookStep {
                id: "s1".to_string(),
                description: "step".to_string(),
                instruction: "Do the step.".to_string(),
                mode: StepMode::Agentic as i32,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Users `alice` and `bob` (members), `pubby` (publisher); a released skill, playbook
    /// and policy, and a draft-only skill and policy owned by `pubby`.
    fn store() -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        store
            .create_user_with_role("pubby", "pubby", crate::users::UserRole::Publisher)
            .expect("pubby");
        store
            .put_skill_package_with_owner(&package(SKILL, "", SkillChannel::Released), "pubby")
            .expect("skill");
        store
            .put_skill_package_with_owner(
                &package(DRAFT_SKILL, "draft", SkillChannel::Draft),
                "pubby",
            )
            .expect("draft skill");
        store
            .put_playbook_with_owner(&playbook(), "pubby")
            .expect("playbook");
        store
            .put_policy_released_with_owner(POLICY, CEDAR, "pubby")
            .expect("policy");
        store
            .publish_policy_draft_with_owner(DRAFT_POLICY, CEDAR, "pubby")
            .expect("draft policy");
        store
    }

    fn agent(urn: &str) -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            display_name: "Member agent".to_string(),
            description: "test".to_string(),
            role: "tester".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions("go".into())),
            allow_any_playbook: true,
            ..Default::default()
        }
    }

    fn save(
        store: &SqliteWorkflowStore,
        who: &str,
        spec: &AgentSpec,
    ) -> Result<AgentSpec, OrchestratorError> {
        store.publish_agent_draft_restricted(spec, who, 50)
    }

    fn message(err: OrchestratorError) -> String {
        match err {
            OrchestratorError::InvalidContract(m)
            | OrchestratorError::Conflict(m)
            | OrchestratorError::PermissionDenied(m) => m,
            other => panic!("unexpected error {other:?}"),
        }
    }

    const ALICE: &str = "urn:vam:agent:local.vam.dev/alice/one";

    #[test]
    fn a_new_agent_in_the_callers_namespace_is_saved() {
        let store = store();
        let mut spec = agent(ALICE);
        spec.skill_refs = vec![rref(SKILL), versioned(SKILL, "")];
        spec.default_playbook_ref = Some(rref(PLAYBOOK));
        spec.permissions = Some(vam_spec::v1::AgentPermissions {
            advanced: vec![rref(POLICY)],
            ..Default::default()
        });
        save(&store, "alice", &spec).expect("floating and X.Y.Z released refs pass");
    }

    #[test]
    fn each_refused_field_is_a_400_naming_it() {
        let store = store();
        let cases: Vec<(&str, SpecEdit)> = vec![
            (
                "capabilityPolicy",
                Box::new(|s| s.capability_policy = Some(CapabilityPolicyRef::default())),
            ),
            (
                "hitlPolicy",
                Box::new(|s| s.hitl_policy = Some(HitlPolicyRef::default())),
            ),
            (
                "instructionArtifact",
                Box::new(|s| {
                    s.instruction_source = Some(InstructionSource::InstructionArtifact(rref(
                        "urn:vam:skill:local.vam.dev/acme/x",
                    )))
                }),
            ),
            (
                "extends",
                Box::new(|s| s.extends = Some(versioned("urn:vam:agent:local.vam.dev/acme/p", ""))),
            ),
            (
                "overlay",
                Box::new(|s| s.overlay = Some(vam_spec::v1::AgentOverlay::default())),
            ),
            (
                "completionPolicy.evaluatorRefs",
                Box::new(|s| {
                    s.completion_policy = Some(CompletionPolicyRef {
                        evaluator_refs: vec![rref(SKILL)],
                        ..Default::default()
                    })
                }),
            ),
        ];
        for (field, edit) in cases {
            let mut spec = agent(ALICE);
            edit(&mut spec);
            let err = message(save(&store, "alice", &spec).expect_err(field));
            assert!(err.contains(&format!("'{field}'")), "{field}: {err}");
        }
        assert!(
            store
                .get_agent_record_visible(ALICE, "alice", false)
                .is_err(),
            "nothing written"
        );
    }

    #[test]
    fn every_ref_kind_is_checked_and_the_five_failure_causes_read_alike() {
        let store = store();
        // The caller's own draft of a skill (alice) and a publisher's draft (pubby).
        store
            .put_skill_package_with_owner(
                &package(
                    "urn:vam:skill:local.vam.dev/acme/alice-draft",
                    "draft",
                    SkillChannel::Draft,
                ),
                "alice",
            )
            .expect("alice draft");
        let skill_causes = [
            rref("urn:vam:skill:local.vam.dev/acme/missing"),
            rref(DRAFT_SKILL),
            rref("urn:vam:skill:local.vam.dev/acme/alice-draft"),
            versioned(DRAFT_SKILL, "draft"),
            versioned("urn:vam:skill:local.vam.dev/acme/alice-draft", "draft"),
        ];
        for bad in skill_causes {
            let urn = bad.resource_id.clone().unwrap().urn;
            let mut spec = agent(ALICE);
            spec.skill_refs = vec![bad];
            let err = message(save(&store, "alice", &spec).expect_err(&urn));
            assert_eq!(
                err,
                format!(
                    "skill '{urn}' has no released version; an agent saved by a member may use \
                     released skills, playbooks and policies only"
                )
            );
        }
        // A version with no release and a digest that is not the release's.
        let mut wrong = versioned(SKILL, "");
        wrong.version.as_mut().unwrap().major = 9;
        let mut spec = agent(ALICE);
        spec.skill_refs = vec![wrong];
        message(save(&store, "alice", &spec).expect_err("no 9.0.0"));
        let mut mismatch = versioned(SKILL, "");
        mismatch.digest = Some(Digest {
            algorithm: 1,
            hex_hash: "b".repeat(64),
        });
        spec.skill_refs = vec![mismatch];
        message(save(&store, "alice", &spec).expect_err("digest"));

        // Each other ref kind.
        let unreleased_policy = || rref(DRAFT_POLICY);
        let slots: Vec<(&str, SpecEdit)> = vec![
            (
                "playbook",
                Box::new(|s| {
                    s.default_playbook_ref =
                        Some(rref("urn:vam:playbook:local.vam.dev/acme/missing"))
                }),
            ),
            (
                "playbook",
                Box::new(|s| {
                    s.allowed_playbook_refs =
                        vec![rref("urn:vam:playbook:local.vam.dev/acme/missing")]
                }),
            ),
            (
                "policy",
                Box::new(move |s| {
                    s.permissions = Some(vam_spec::v1::AgentPermissions {
                        advanced: vec![unreleased_policy()],
                        ..Default::default()
                    })
                }),
            ),
            (
                "policy",
                Box::new(|s| {
                    s.telemetry_policy = Some(TelemetryPolicyRef {
                        policy_ref: Some(rref(DRAFT_POLICY)),
                        ..Default::default()
                    })
                }),
            ),
            (
                "policy",
                Box::new(|s| {
                    s.completion_policy = Some(CompletionPolicyRef {
                        policy_ref: Some(rref(DRAFT_POLICY)),
                        ..Default::default()
                    })
                }),
            ),
        ];
        for (noun, edit) in slots {
            let mut spec = agent(ALICE);
            edit(&mut spec);
            let err = message(save(&store, "alice", &spec).expect_err(noun));
            assert!(
                err.starts_with(&format!("{noun} '")) && err.contains("has no released version"),
                "{err}"
            );
        }
        assert!(store
            .get_agent_record_visible(ALICE, "alice", false)
            .is_err());
    }

    #[test]
    fn namespace_new_inside_ok_outside_400_non_slug_username_uses_default() {
        let store = store();
        save(&store, "alice", &agent(ALICE)).expect("inside");
        let err = message(
            save(
                &store,
                "alice",
                &agent("urn:vam:agent:local.vam.dev/acme/two"),
            )
            .expect_err("outside"),
        );
        assert!(err.contains("namespace"), "{err}");
        assert!(err.contains("urn:vam:agent:local.vam.dev/alice/"), "{err}");
        // Another registry domain in the caller's namespace is outside it too.
        message(
            save(
                &store,
                "alice",
                &agent("urn:vam:agent:other.example/alice/two"),
            )
            .expect_err("domain"),
        );
        // A username that is not a slug uses `default`.
        store.create_user("u-9", "_odd").expect("odd username");
        save(
            &store,
            "u-9",
            &agent("urn:vam:agent:local.vam.dev/default/odd"),
        )
        .expect("default namespace");
        message(
            save(&store, "u-9", &agent("urn:vam:agent:local.vam.dev/u-9/odd"))
                .expect_err("user id is not the namespace"),
        );
    }

    #[test]
    fn another_members_urn_is_a_400_and_stays_unchanged() {
        let store = store();
        let first = save(&store, "alice", &agent(ALICE)).expect("alice");
        let mut theirs = agent(ALICE);
        theirs.description = "bob was here".to_string();
        let err = message(save(&store, "bob", &theirs).expect_err("bob"));
        assert!(err.contains("namespace"), "{err}");
        let kept = store.get_agent_visible(ALICE, "alice", false).expect("get");
        assert_eq!(kept.description, first.description);
    }

    #[test]
    fn a_urn_in_my_namespace_owned_by_a_publisher_is_403() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/bob/pubs";
        store
            .publish_agent_draft_with_owner(&agent(urn), "pubby")
            .expect("publisher draft");
        let err = message(save(&store, "bob", &agent(urn)).expect_err("owned by pubby"));
        assert!(err.contains("only the artifact owner"), "{err}");
    }

    #[test]
    fn a_row_with_an_unset_or_empty_owner_reads_as_local_and_cannot_be_claimed() {
        let store = store();
        let blanks = [
            (
                "null-owner",
                "UPDATE agents SET owner_user_id = NULL WHERE urn = ?1",
            ),
            (
                "empty-owner",
                "UPDATE agents SET owner_user_id = '' WHERE urn = ?1",
            ),
        ];
        for (name, sql) in blanks {
            let urn = format!("urn:vam:agent:local.vam.dev/bob/{name}");
            store
                .publish_agent_draft_with_owner(&agent(&urn), "bob")
                .expect("row");
            {
                // Blank the owner as a legacy row would be (no FK for the empty string).
                let connection = store.connection.lock().expect("lock");
                connection
                    .execute_batch("PRAGMA foreign_keys = OFF")
                    .expect("fk off");
                connection.execute(sql, [&urn]).expect("blank owner");
                connection
                    .execute_batch("PRAGMA foreign_keys = ON")
                    .expect("fk on");
            }
            let err = message(save(&store, "bob", &agent(&urn)).expect_err(name));
            assert!(err.contains("only the artifact owner"), "{name}: {err}");
            // `local` owns an empty-owner row, so it may edit. (A NULL owner cannot survive a
            // start: `initialize_owner_stamps` stamps it `local`, and the draft reader does
            // not accept one, so only the refusal above is asserted for it.)
            if name == "null-owner" {
                continue;
            }
            let mut again = agent(&urn);
            again.description = "local edit".to_string();
            store
                .publish_agent_draft_restricted(&again, "local", 50)
                .unwrap_or_else(|e| panic!("{name}: local owns an unowned row: {e}"));
        }
    }

    #[test]
    fn editing_an_own_draft_outside_the_namespace_is_allowed() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/former-publisher";
        store
            .publish_agent_draft_with_owner(&agent(urn), "alice")
            .expect("alice wrote it when she was a publisher");
        let mut edit = agent(urn);
        edit.description = "edited".to_string();
        edit.skill_refs = vec![rref(SKILL)];
        let saved = save(&store, "alice", &edit).expect("own draft, released refs");
        assert_eq!(saved.description, "edited");
        edit.skill_refs = vec![rref(DRAFT_SKILL)];
        message(save(&store, "alice", &edit).expect_err("draft ref on edit"));
    }

    #[test]
    fn the_cap_counts_drafts_only_and_refuses_a_new_row_not_an_edit() {
        let store = store();
        let urn = |n: usize| format!("urn:vam:agent:local.vam.dev/alice/a{n}");
        let max = 3;
        for n in 0..max {
            store
                .publish_agent_draft_restricted(&agent(&urn(n)), "alice", max)
                .expect("under the cap");
        }
        // Released rows do not count: alice's released agent takes no draft slot.
        let err = message(
            store
                .publish_agent_draft_restricted(&agent(&urn(99)), "alice", max)
                .expect_err("over the cap"),
        );
        assert!(err.contains("at most 3"), "{err}");
        assert!(matches!(
            store.publish_agent_draft_restricted(&agent(&urn(98)), "alice", max),
            Err(OrchestratorError::Conflict(_))
        ));
        let mut edit = agent(&urn(0));
        edit.description = "edit at the cap".to_string();
        store
            .publish_agent_draft_restricted(&edit, "alice", max)
            .expect("an edit is not a new row");
        // Another member has their own count.
        store
            .publish_agent_draft_restricted(
                &agent("urn:vam:agent:local.vam.dev/bob/a0"),
                "bob",
                max,
            )
            .expect("bob");
    }

    #[test]
    fn a_released_agent_row_does_not_count_toward_the_cap() {
        let store = store();
        store
            .put_agent_released_with_owner(
                &agent("urn:vam:agent:local.vam.dev/alice/shipped"),
                "alice",
            )
            .expect("released");
        store
            .publish_agent_draft_restricted(&agent(ALICE), "alice", 1)
            .expect("the only draft");
    }

    #[test]
    fn concurrent_creates_at_the_limit_leave_at_most_the_limit() {
        let store = Arc::new(store());
        let max = 5;
        let threads: Vec<_> = (0..16)
            .map(|n| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    store
                        .publish_agent_draft_restricted(
                            &agent(&format!("urn:vam:agent:local.vam.dev/alice/c{n}")),
                            "alice",
                            max,
                        )
                        .is_ok()
                })
            })
            .collect();
        let created = threads
            .into_iter()
            .map(|t| t.join().expect("thread"))
            .filter(|ok| *ok)
            .count();
        assert_eq!(created, max);
        let connection = store.connection.lock().expect("lock");
        let drafts: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM agents WHERE channel='draft' AND owner_user_id='alice'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(drafts as usize, max);
    }

    #[test]
    fn the_permissions_edit_of_an_own_draft_is_checked_and_leaves_the_draft() {
        let store = store();
        let mut spec = agent(ALICE);
        spec.skill_refs = vec![rref(SKILL)];
        save(&store, "alice", &spec).expect("save");
        let before = store.get_agent_visible(ALICE, "alice", false).expect("get");
        let err = crate::catalog_publish::update_draft_permissions_restricted(
            &store,
            ALICE,
            "alice",
            50,
            |permissions| permissions.advanced = vec![rref(DRAFT_POLICY)],
        )
        .expect_err("draft advanced policy");
        assert!(message(err).contains("has no released version"));
        let after = store.get_agent_visible(ALICE, "alice", false).expect("get");
        assert_eq!(before, after, "the draft is unchanged");
        crate::catalog_publish::update_draft_permissions_restricted(
            &store,
            ALICE,
            "alice",
            50,
            |permissions| permissions.advanced = vec![rref(POLICY)],
        )
        .expect("released advanced policy");
    }

    #[test]
    fn always_allow_cannot_add_a_skill_playbook_or_policy_ref() {
        // Always allow and deny use the unvalidated writer. It hands the closure only
        // `&mut AgentPermissions` and keeps every other field of the stored draft, so the
        // refs on the draft (`skill_refs`, playbooks, the legacy policy refs) cannot change;
        // the one ref-bearing permissions field, `advanced`, is left as it was.
        let store = store();
        let mut spec = agent(ALICE);
        spec.skill_refs = vec![rref(SKILL)];
        spec.permissions = Some(vam_spec::v1::AgentPermissions {
            advanced: vec![rref(POLICY)],
            ..Default::default()
        });
        save(&store, "alice", &spec).expect("save");
        let before = store.get_agent_visible(ALICE, "alice", false).expect("get");
        let saved = crate::catalog_publish::update_draft_permissions(
            &store,
            ALICE,
            "alice",
            |permissions| {
                crate::catalog_publish::upsert_permission_rule(
                    permissions,
                    vam_spec::v1::PermissionRule {
                        target: "skill:released-skill/search".to_string(),
                        decision: vam_spec::v1::PermissionDecision::Allow as i32,
                    },
                )
            },
        )
        .expect("always allow");
        assert_eq!(saved.skill_refs, before.skill_refs);
        assert_eq!(saved.allowed_playbook_refs, before.allowed_playbook_refs);
        assert_eq!(saved.default_playbook_ref, before.default_playbook_ref);
        assert_eq!(
            saved.permissions.as_ref().map(|p| &p.advanced),
            before.permissions.as_ref().map(|p| &p.advanced)
        );
    }

    #[test]
    fn cap_parses_a_positive_number_else_the_default() {
        assert_eq!(parse_member_draft_agents_max(None), 50);
        assert_eq!(parse_member_draft_agents_max(Some(" 7 ")), 7);
        assert_eq!(parse_member_draft_agents_max(Some("0")), 50);
        assert_eq!(parse_member_draft_agents_max(Some("many")), 50);
        assert_eq!(parse_member_draft_agents_max(Some("-3")), 50);
    }
}
