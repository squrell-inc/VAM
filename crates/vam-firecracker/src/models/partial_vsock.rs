use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Partial update payload for PATCH `/vsock` (typically `uds_path` rebinding after snapshot load).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartialVsock {
    #[serde(rename = "uds_path", skip_serializing_if = "Option::is_none")]
    pub uds_path: Option<PathBuf>,
}
