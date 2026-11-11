fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Locate the protoc binary from protoc-bin-vendored if PROTOC is not set
    let protoc_path = std::env::var("PROTOC").ok().unwrap_or_else(|| {
        let path = protoc_bin_vendored::protoc_bin_path().unwrap();
        path.to_string_lossy().into_owned()
    });
    std::env::set_var("PROTOC", protoc_path);

    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    embed_builtin_connectors(&manifest_dir)?;
    let proto_dir = manifest_dir.join("../../proto");

    let proto_files = &[
        proto_dir.join("vam/v1/common.proto"),
        proto_dir.join("vam/v1/skills.proto"),
        proto_dir.join("vam/v1/model_policy.proto"),
        proto_dir.join("vam/v1/agentic_spec.proto"),
        proto_dir.join("vam/v1/workflow.proto"),
        proto_dir.join("vam/v1/hitl.proto"),
        proto_dir.join("vam/v1/evals.proto"),
        proto_dir.join("vam/v1/events.proto"),
        proto_dir.join("vam/v1/telemetry.proto"),
        proto_dir.join("vam/v1/security.proto"),
        proto_dir.join("vam/v1/vsock_rpc.proto"),
        proto_dir.join("vam/v1/storage.proto"),
        proto_dir.join("vam/v1/network.proto"),
        proto_dir.join("vam/v1/inference.proto"),
        proto_dir.join("vam/v1/orchestrator.proto"),
        proto_dir.join("vam/v1/hosted_services.proto"),
        proto_dir.join("vam/v1/receipt.proto"),
        proto_dir.join("vam/v1/outcomes.proto"),
    ];

    for proto_file in proto_files {
        println!("cargo:rerun-if-changed={}", proto_file.display());
    }
    println!("cargo:rerun-if-changed={}", proto_dir.display());

    let descriptor_path =
        std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("vam_descriptor.bin");

    let build_client = std::env::var("CARGO_FEATURE_CLIENT").is_ok();
    let build_server = std::env::var("CARGO_FEATURE_SERVER").is_ok();

    tonic_build::configure()
        .build_client(build_client)
        .build_server(build_server)
        .file_descriptor_set_path(&descriptor_path)
        .type_attribute(
            ".vam.v1.ResourceEnvelope",
            "#[derive(serde::Serialize, serde::Deserialize, Eq)]",
        )
        .type_attribute(
            ".vam.v1.ResourceUsage",
            "#[derive(serde::Serialize, serde::Deserialize, Eq)]",
        )
        .type_attribute(
            ".vam.v1.OutputBlock.block",
            "#[allow(clippy::large_enum_variant)]",
        )
        .compile(proto_files, &[proto_dir])?;

    Ok(())
}

/// Embed every `spec/connectors/*.json` as `BUILTIN_CONNECTOR_FILES` (file name, text), sorted by
/// file name. Adding a connector is adding a file: no Rust edit.
fn embed_builtin_connectors(
    manifest_dir: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = manifest_dir.join("../../spec/connectors").canonicalize()?;
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") && path.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    let mut out = String::from("pub const BUILTIN_CONNECTOR_FILES: &[(&str, &str)] = &[\n");
    for name in &names {
        let abs = dir.join(name);
        println!("cargo:rerun-if-changed={}", abs.display());
        out.push_str(&format!(
            "    ({name:?}, include_str!({:?})),\n",
            abs.to_string_lossy()
        ));
    }
    out.push_str("];\n");
    let dest = std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("builtin_connectors.rs");
    std::fs::write(dest, out)?;
    Ok(())
}
