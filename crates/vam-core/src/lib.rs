//! # Virtual Agentic Machine (VAM)
//!
//! VAM is a Rust workspace for designing a virtual machine stack tailored to
//! agentic workloads — microVM isolation, storage, and orchestration for
//! autonomous agents.
//!
//! This crate is the core library entry point. Firecracker integration, Drive9
//! storage, and higher-level orchestration will land here as the design matures.
//!
//! See the project docs at `docs/` and `AGENTS.md` for architecture and
//! orientation.

pub mod cid;
pub mod config;
pub mod jail;
#[cfg(feature = "process-guest")]
pub mod process_vmm;
pub mod state;
pub mod vmm;

pub use cid::CidAllocator;
pub use config::{DriveAttachment, MicroVmConfig, MicroVmConfigBuilder};
pub use jail::{JailConfig, JailPreflightError, JailRefusal, PreflightInputs};
#[cfg(feature = "process-guest")]
pub use process_vmm::ProcessVmm;
pub use state::{VmState, VmStateMachine};
pub use vmm::{record_timing_step, FirecrackerVmm, MockVmm, Vmm};

use thiserror::Error;

/// Placeholder version string for the VAM core library.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Error, Debug)]
pub enum VamCoreError {
    #[error("KVM is unavailable on the host: {reason}")]
    KvmUnavailable { reason: String },

    #[error("Jailer sandbox setup failed: {details}")]
    JailerSetup { details: String },

    #[error("Failed to spawn Firecracker process: {0}")]
    SpawnFailed(#[from] std::io::Error),

    #[error("Timeout waiting for Firecracker API socket readiness")]
    ApiSocketTimeout,

    #[error("Firecracker REST API request failed: HTTP {status} - {message}")]
    ApiRequestFailed { status: u16, message: String },

    #[error("Configuration validation failed: {0}")]
    InvalidConfig(String),

    #[error("Guest CID namespace exhausted")]
    CidExhausted,

    #[error("MicroVM snapshot failed: {0}")]
    SnapshotFailed(String),

    #[error("MicroVM process crashed unexpectedly: Exit Code {0}")]
    ProcessCrashed(i32),

    #[error("Failed to clean up VM resources: {0}")]
    CleanupFailed(String),

    #[error("Firecracker SDK error: {0}")]
    FirecrackerSdk(#[from] vam_firecracker::Error),
}
