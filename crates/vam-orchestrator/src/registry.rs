//! This node's registry domains, installed once by `vamd` at startup.
//!
//! A process-global like [`crate::brokers::install_egress_max_bytes`]: the
//! value is fixed for the life of the process and read from deep inside
//! admission and skill resolution, which carry no config. The default is a
//! valid state (`local.vam.dev`, no marketplace).

use std::sync::{Arc, RwLock};

use vam_spec::RegistryDomains;

static DOMAINS: RwLock<Option<Arc<RegistryDomains>>> = RwLock::new(None);

/// Install the validated domains for this process.
pub fn install_registry_domains(domains: RegistryDomains) {
    let mut slot = DOMAINS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(Arc::new(domains));
}

/// The installed domains, or the unconfigured default.
pub fn registry_domains() -> Arc<RegistryDomains> {
    let slot = DOMAINS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    slot.clone()
        .unwrap_or_else(|| Arc::new(RegistryDomains::default()))
}
