use prost::Message;
use std::fs;
use std::path::{Path, PathBuf};
use vam_spec::v1::{
    agent_spec::InstructionSource, AgentSpec, CapabilityManifest, Digest, FileRef,
    HookParticipation, ManifestSecretBinding, PlaybookSpec, PlaybookStep, ResourceId, ResourceRef,
    RunSpec, Signature, SignedEnvelope, SkillManifest, StepKind, StepMode, ToolExport, ToolSpec,
    Version,
};

fn save_fixture<T: Message>(
    proto_name: &str,
    name: &str,
    msg: &T,
    baseline_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let desc = vam_spec::encoding::DESCRIPTOR_POOL
        .get_message_by_name(proto_name)
        .ok_or_else(|| format!("message '{}' not found", proto_name))?;

    let mut buf = Vec::new();
    msg.encode(&mut buf)?;

    let dynamic = prost_reflect::DynamicMessage::decode(desc, &*buf)?;

    // Save binary
    fs::write(baseline_dir.join(format!("{}.bin", name)), &buf)?;

    // Save JSON
    let mut out = Vec::new();
    let mut serializer = serde_json::Serializer::new(&mut out);
    serde::Serialize::serialize(&dynamic, &mut serializer)?;
    let json_str = String::from_utf8(out)?;
    fs::write(baseline_dir.join(format!("{}.json", name)), json_str)?;

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let baseline_dir = manifest_dir.join("../../proto/vam/v1/baseline");
    fs::create_dir_all(&baseline_dir)?;

    // 1. Copy the current descriptor set to the baseline directory
    let descriptor_src = PathBuf::from(env!("OUT_DIR")).join("vam_descriptor.bin");
    fs::copy(&descriptor_src, baseline_dir.join("vam_descriptor.bin"))?;
    println!("Copied descriptor set to baseline.");

    // --- CREATE VALID FIXTURES ---

    // 1. Valid AgentSpec
    let valid_agent = AgentSpec {
        resource_id: Some(ResourceId {
            urn: "urn:vam:agent:hub.vam.dev/acme/oracle-developer".to_string(),
        }),
        display_name: "Oracle Developer".to_string(),
        description: "An agent that writes PL/SQL and manages ERP schemas.".to_string(),
        role: "Oracle Developer".to_string(),
        instruction_source: Some(InstructionSource::SystemInstructions(
            "You are a helpful Oracle Developer agent.".to_string(),
        )),
        model_requirements: None,
        preferred_models: vec![],
        context_policy: Some(vam_spec::v1::ContextPolicy {
            max_token_count: 128000,
            max_history_turns: 50,
            compaction: 1, // TRUNCATE_FIFO
        }),
        reasoning_policy: Some(vam_spec::v1::ReasoningPolicy {
            max_iterations: 20,
            step_timeout: Some(prost_types::Duration {
                seconds: 300,
                nanos: 0,
            }),
            termination: 1, // EXPLICIT_CLAIM
        }),
        skill_refs: vec![],
        capability_policy: None,
        hitl_policy: None,
        telemetry_policy: None,
        resource_requirements: None,
        completion_policy: None,
        default_playbook_ref: None,
        allowed_playbook_refs: vec![],
        allow_any_playbook: true,
        metadata: None,
        extends: None,
        overlay: None,
        ..Default::default()
    };
    save_fixture(
        "vam.v1.AgentSpec",
        "agent_spec_valid",
        &valid_agent,
        &baseline_dir,
    )?;

    // 2. Valid SkillManifest
    let valid_skill = SkillManifest {
        resource_id: Some(ResourceId {
            urn: "urn:vam:skill:hub.vam.dev/acme/oracle-sql".to_string(),
        }),
        version: Some(Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: String::new(),
            build: String::new(),
        }),
        description: "Enables PL/SQL query execution and schema inspections.".to_string(),
        system_extension: "You can execute PL/SQL queries using the oracle_sql_execute tool."
            .to_string(),
        tools: vec![ToolSpec {
            name: "oracle_sql_execute".to_string(),
            description: "Execute a PL/SQL command on the target database.".to_string(),
            input_schema_json: r#"{"type": "object", "properties": {"query": {"type": "string"}}}"#
                .to_string(),
            execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::LocalExec(
                vam_spec::v1::LocalExecSpec {
                    command: "node".to_string(),
                    entrypoint: "./index.js".to_string(),
                    default_args: vec![],
                    env_vars: std::collections::HashMap::new(),
                },
            )),
            required_secrets: vec![],
            side_effect: 3, // SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL
            output_schema_json: "".to_string(),
            timeout: Some(prost_types::Duration {
                seconds: 30,
                nanos: 0,
            }),
            ..Default::default()
        }],
        runtime_compatibility: None,
        metadata: None,
        locators: vec![],
        capabilities: vec![],
        policy: String::new(),
        playbooks: vec![],
        ..Default::default()
    };
    save_fixture(
        "vam.v1.SkillManifest",
        "skill_manifest_valid",
        &valid_skill,
        &baseline_dir,
    )?;

    // 3. Valid PlaybookSpec
    let valid_playbook = PlaybookSpec {
        resource_id: Some(ResourceId {
            urn: "urn:vam:playbook:hub.vam.dev/acme/erp-monthly-close".to_string(),
        }),
        display_name: "ERP Monthly Close".to_string(),
        description: "Executes monthly closing procedures.".to_string(),
        steps: vec![
            PlaybookStep {
                id: "step1".to_string(),
                description: "Inspect schema".to_string(),
                instruction: "Inspect the schema.".to_string(),
                mode: StepMode::Agentic as i32,
                kind: StepKind::Task as i32,
                ..Default::default()
            },
            PlaybookStep {
                id: "step2".to_string(),
                description: "Run closing SQL".to_string(),
                instruction: "Run the closing SQL.".to_string(),
                mode: StepMode::Agentic as i32,
                kind: StepKind::Task as i32,
                after: vec!["step1".to_string()],
                ..Default::default()
            },
        ],
        input_schema: None,
        output_schema: None,
        metadata: None,
        requires: None,
        guards: vec![],
        activation: None,
        guidance: String::new(),
    };
    save_fixture(
        "vam.v1.PlaybookSpec",
        "playbook_spec_valid",
        &valid_playbook,
        &baseline_dir,
    )?;

    // 4. Valid RunSpec
    let valid_run = RunSpec {
        inbox_mode: 0,
        space_id: String::new(),
        thread_id: String::new(),
        resolved_refs: vec![],
        run_id: Some(ResourceId {
            urn: "urn:vam:run:control.vam.dev/acme/run-abc-123".to_string(),
        }),
        agent_ref: Some(ResourceRef {
            resource_id: Some(ResourceId {
                urn: "urn:vam:agent:hub.vam.dev/acme/oracle-developer".to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            }),
            digest: Some(Digest {
                algorithm: 1,
                hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_string(),
            }),
            locators: vec![],
        }),
        playbook_ref: Some(ResourceRef {
            resource_id: Some(ResourceId {
                urn: "urn:vam:playbook:hub.vam.dev/acme/erp-monthly-close".to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            }),
            digest: Some(Digest {
                algorithm: 1,
                hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_string(),
            }),
            locators: vec![],
        }),
        inputs: None,
        max_duration: Some(prost_types::Duration {
            seconds: 3600,
            nanos: 0,
        }),
        max_financial_budget: Some(vam_spec::v1::Cost {
            currency: "USD".to_string(),
            micro_amount: 10000000,
        }),
        workspace_ref_uri: "volume://drive9/acme-closing-workspace".to_string(),
        idempotency_key: "idemp-key-789".to_string(),
        thread_ref: String::new(),
    };
    save_fixture(
        "vam.v1.RunSpec",
        "run_spec_valid",
        &valid_run,
        &baseline_dir,
    )?;

    // 5. Valid SignedEnvelope
    let valid_envelope = SignedEnvelope {
        domain: "vam.v1.hitl-token".to_string(),
        envelope_version: Some(Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: String::new(),
            build: String::new(),
        }),
        claims: vec![1, 2, 3, 4],
        signature: Some(Signature {
            algorithm: 1, // SIGNATURE_ALGORITHM_ED25519
            key_id: "key-123".to_string(),
            value: vec![12, 13, 14, 15],
        }),
    };
    save_fixture(
        "vam.v1.SignedEnvelope",
        "signed_envelope_valid",
        &valid_envelope,
        &baseline_dir,
    )?;

    // 6. Valid CapabilityManifest
    let valid_capability_manifest = CapabilityManifest {
        broker_id: "vam.files".to_string(),
        trust_tier: 2, // TRUST_TIER_FIRST_PARTY
        secret_bindings: vec![ManifestSecretBinding {
            logical_name: "storage-token".to_string(),
            vault_key: "vault/files/storage-token".to_string(),
            required: true,
        }],
        egress_domains: vec!["files.example.com".to_string()],
        hooks: vec![
            HookParticipation {
                phase: 1, // HOOK_PHASE_ON_REQUEST
                required: true,
            },
            HookParticipation {
                phase: 3, // HOOK_PHASE_ON_RESPONSE
                required: true,
            },
        ],
        tools: vec![
            ToolExport {
                tool_name: "files.get".to_string(),
                side_effect: 1, // SIDE_EFFECT_CLASS_READ_ONLY
                required_secrets: vec![],
                destination_policy: Default::default(),
                effect: None,
            },
            ToolExport {
                tool_name: "files.put".to_string(),
                side_effect: 3, // SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL
                required_secrets: vec!["storage-token".to_string()],
                destination_policy: Default::default(),
                effect: None,
            },
        ],
        depends_on: vec![],
        labels: std::collections::HashMap::from([("tier".to_string(), "first-party".to_string())]),
        identity_requirement: None,
        run_request_permission: None,
        notification_channel: None,
    };
    save_fixture(
        "vam.v1.CapabilityManifest",
        "capability_manifest_valid",
        &valid_capability_manifest,
        &baseline_dir,
    )?;

    // 6b. Valid CapabilityManifest that may ask for runs (E5)
    let mut run_request_manifest = valid_capability_manifest.clone();
    run_request_manifest.run_request_permission = Some(vam_spec::v1::RunRequestPermission {
        grant_tools: vec!["files.put".to_string()],
        max_requests_per_minute: 0,
        max_requests_per_day: 0,
        loop_guarded_tools: vec!["files.put".to_string()],
        surface: "files".to_string(),
        max_active_grants_per_user: 0,
    });
    save_fixture(
        "vam.v1.CapabilityManifest",
        "capability_manifest_run_request_valid",
        &run_request_manifest,
        &baseline_dir,
    )?;

    // 7. Valid FileRef
    let valid_file_ref = FileRef {
        file_id: Some(ResourceId {
            urn: "urn:vam:file:hub.vam.dev/acme/quarterly-report".to_string(),
        }),
        digest: Some(Digest {
            algorithm: 1,
            hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .to_string(),
        }),
        media_type: "application/pdf".to_string(),
        size_bytes: 4096,
        metadata: None,
    };
    save_fixture(
        "vam.v1.FileRef",
        "file_ref_valid",
        &valid_file_ref,
        &baseline_dir,
    )?;

    // --- CREATE INVALID FIXTURES ---

    // 0. Invalid run_request_permission: no grant tools
    let mut invalid_run_request = run_request_manifest.clone();
    if let Some(permission) = invalid_run_request.run_request_permission.as_mut() {
        permission.grant_tools.clear();
    }
    save_fixture(
        "vam.v1.CapabilityManifest",
        "capability_manifest_invalid_run_request",
        &invalid_run_request,
        &baseline_dir,
    )?;

    // 1. Invalid AgentSpec (Invalid ID)
    let invalid_agent_id = AgentSpec {
        resource_id: Some(ResourceId {
            urn: "urn:invalid:agent:format".to_string(),
        }),
        ..valid_agent.clone()
    };
    save_fixture(
        "vam.v1.AgentSpec",
        "agent_spec_invalid_id",
        &invalid_agent_id,
        &baseline_dir,
    )?;

    // 2. Invalid PlaybookSpec (Cyclic step)
    let invalid_playbook_cycle = PlaybookSpec {
        resource_id: Some(ResourceId {
            urn: "urn:vam:playbook:hub.vam.dev/acme/invalid-cycle".to_string(),
        }),
        steps: vec![
            PlaybookStep {
                id: "step1".to_string(),
                description: "Inspect schema".to_string(),
                instruction: "Inspect the schema.".to_string(),
                mode: StepMode::Agentic as i32,
                kind: StepKind::Task as i32,
                after: vec!["step2".to_string()],
                ..Default::default()
            },
            PlaybookStep {
                id: "step2".to_string(),
                description: "Run closing SQL".to_string(),
                instruction: "Run the closing SQL.".to_string(),
                mode: StepMode::Agentic as i32,
                kind: StepKind::Task as i32,
                after: vec!["step1".to_string()],
                ..Default::default()
            },
        ],
        ..valid_playbook.clone()
    };
    save_fixture(
        "vam.v1.PlaybookSpec",
        "playbook_spec_invalid_cycle",
        &invalid_playbook_cycle,
        &baseline_dir,
    )?;

    // 3. Invalid SkillManifest (Duplicate/invalid tools)
    let invalid_skill_tools = SkillManifest {
        tools: vec![
            ToolSpec {
                name: "oracle_sql_execute".to_string(),
                description: "Run SQL queries".to_string(),
                input_schema_json: "{}".to_string(),
                execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::LocalExec(
                    vam_spec::v1::LocalExecSpec {
                        command: "node".to_string(),
                        entrypoint: "index.js".to_string(),
                        default_args: vec![],
                        env_vars: std::collections::HashMap::new(),
                    },
                )),
                required_secrets: vec![],
                side_effect: 1,
                output_schema_json: "".to_string(),
                timeout: None,
                ..Default::default()
            },
            ToolSpec {
                name: "oracle_sql_execute".to_string(), // Duplicate name!
                description: "Run SQL queries".to_string(),
                input_schema_json: "{}".to_string(),
                execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::LocalExec(
                    vam_spec::v1::LocalExecSpec {
                        command: "node".to_string(),
                        entrypoint: "index.js".to_string(),
                        default_args: vec![],
                        env_vars: std::collections::HashMap::new(),
                    },
                )),
                required_secrets: vec![],
                side_effect: 1,
                output_schema_json: "".to_string(),
                timeout: None,
                ..Default::default()
            },
        ],
        ..valid_skill.clone()
    };
    save_fixture(
        "vam.v1.SkillManifest",
        "skill_manifest_invalid_tools",
        &invalid_skill_tools,
        &baseline_dir,
    )?;

    // 4. Invalid SignedEnvelope (Empty Claims / No Signature)
    let invalid_envelope_sig = SignedEnvelope {
        claims: vec![],  // Empty claims!
        signature: None, // Missing signature!
        ..valid_envelope.clone()
    };
    save_fixture(
        "vam.v1.SignedEnvelope",
        "signed_envelope_invalid_signature",
        &invalid_envelope_sig,
        &baseline_dir,
    )?;

    // 5. Invalid CapabilityManifest (empty broker_id)
    let invalid_capability_manifest = CapabilityManifest {
        broker_id: String::new(),
        ..valid_capability_manifest.clone()
    };
    save_fixture(
        "vam.v1.CapabilityManifest",
        "capability_manifest_invalid_broker_id",
        &invalid_capability_manifest,
        &baseline_dir,
    )?;

    // 6. Invalid FileRef (two-segment URN path)
    let invalid_file_ref = FileRef {
        file_id: Some(ResourceId {
            urn: "urn:vam:file:acme/550e8400-e29b-41d4-a716-446655440000".to_string(),
        }),
        ..valid_file_ref.clone()
    };
    save_fixture(
        "vam.v1.FileRef",
        "file_ref_invalid_urn",
        &invalid_file_ref,
        &baseline_dir,
    )?;

    println!("All baseline fixtures successfully generated and saved to 'proto/vam/v1/baseline/'.");
    Ok(())
}
