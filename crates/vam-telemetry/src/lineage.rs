//! OpenLineage-compatible provenance models and builders (Phase 6.3).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const OPENLINEAGE_PRODUCER: &str = "https://github.com/squrell-inc/VAM/telemetry/v1";
pub const OPENLINEAGE_SCHEMA_URL: &str = "https://openlineage.io/spec/1-0-5/OpenLineage.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OpenLineageRun {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub facets: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OpenLineageJob {
    pub namespace: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub facets: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OpenLineageDataset {
    pub namespace: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub facets: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OpenLineageRunEvent {
    pub event_type: String, // "START", "RUNNING", "COMPLETE", "FAIL", "ABORT"
    pub event_time: String, // ISO-8601 UTC
    pub run: OpenLineageRun,
    pub job: OpenLineageJob,
    pub inputs: Vec<OpenLineageDataset>,
    pub outputs: Vec<OpenLineageDataset>,
    pub producer: String,
    pub schema_url: String,
}

impl OpenLineageRunEvent {
    /// Creates a new OpenLineage run event.
    pub fn new(
        event_type: impl Into<String>,
        event_time: impl Into<String>,
        run_id: impl Into<String>,
        job_namespace: impl Into<String>,
        job_name: impl Into<String>,
        inputs: Vec<OpenLineageDataset>,
        outputs: Vec<OpenLineageDataset>,
    ) -> Self {
        Self {
            event_type: event_type.into(),
            event_time: event_time.into(),
            run: OpenLineageRun {
                run_id: run_id.into(),
                facets: HashMap::new(),
            },
            job: OpenLineageJob {
                namespace: job_namespace.into(),
                name: job_name.into(),
                facets: HashMap::new(),
            },
            inputs,
            outputs,
            producer: OPENLINEAGE_PRODUCER.to_string(),
            schema_url: OPENLINEAGE_SCHEMA_URL.to_string(),
        }
    }
}

/// Helper to build an input dataset from a knowledge reference or knowledge item.
pub fn build_knowledge_input_dataset(name: &str, digest: &str) -> OpenLineageDataset {
    let mut facets = HashMap::new();
    facets.insert(
        "knowledge".to_string(),
        serde_json::json!({
            "digest": digest,
            "category": "instruction_knowledge",
        }),
    );

    OpenLineageDataset {
        namespace: "vam.knowledge".to_string(),
        name: name.to_string(),
        facets,
    }
}

/// Helper to build a Drive9 dataset facet (workspace files, snapshot manifests, or CAS chunks).
pub fn build_drive9_dataset(
    name: &str,
    manifest_hash: &str,
    size_bytes: Option<u64>,
    chunk_count: Option<usize>,
) -> OpenLineageDataset {
    let mut facets = HashMap::new();
    let mut storage_json = serde_json::json!({
        "manifestHash": manifest_hash,
        "storageEngine": "drive9-ext4-cas",
    });

    if let Some(bytes) = size_bytes {
        storage_json["sizeBytes"] = serde_json::json!(bytes);
    }
    if let Some(chunks) = chunk_count {
        storage_json["chunkCount"] = serde_json::json!(chunks);
    }

    facets.insert("storage".to_string(), storage_json);

    OpenLineageDataset {
        namespace: "vam.drive9".to_string(),
        name: name.to_string(),
        facets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openlineage_event_serialization() {
        let input_knowledge = build_knowledge_input_dataset("ref/react-agent", "sha256:11223344");
        let input_manifest =
            build_drive9_dataset("workspace-initial", "manifest-base-1", Some(1024), Some(1));
        let output_manifest =
            build_drive9_dataset("workspace-final", "manifest-head-2", Some(4096), Some(4));

        let event = OpenLineageRunEvent::new(
            "COMPLETE",
            "2026-09-08T05:00:00.000Z",
            "urn:vam:run:run-1234",
            "vam.agent",
            "urn:vam:agent:react-coder",
            vec![input_knowledge, input_manifest],
            vec![output_manifest],
        );

        assert_eq!(event.event_type, "COMPLETE");
        assert_eq!(event.inputs.len(), 2);
        assert_eq!(event.outputs.len(), 1);

        let json = serde_json::to_string(&event).expect("json serialization should succeed");
        assert!(json.contains("\"eventType\":\"COMPLETE\""));
        assert!(json.contains("\"namespace\":\"vam.knowledge\""));
        assert!(json.contains("\"manifestHash\":\"manifest-head-2\""));
        assert!(json.contains(OPENLINEAGE_PRODUCER));
    }
}
