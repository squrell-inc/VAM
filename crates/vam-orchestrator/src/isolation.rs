//! The isolation this node runs, built from its running config.
//!
//! `describe` is the one function that turns config into the isolation line.
//! `GET /v1/node` calls it today; the run receipt will call the same function.
//! The line is never a constant: it follows the inputs `vamd` installs at start.

use std::sync::OnceLock;

use vam_spec::v1::NodeIsolation;

/// What the running node is configured to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsolationInputs {
    /// `"vm"` or `"process"` (see [`crate::guest_mode`]).
    pub guest_mode: &'static str,
    pub use_jailer: bool,
    pub api_tls: bool,
    /// The API rate limits the node enforces (IT-2.4).
    pub rate_limits: crate::api::rate_limit::RateLimitConfig,
}

static INSTALLED: OnceLock<IsolationInputs> = OnceLock::new();

/// Records the running config's inputs (once) and logs them.
pub fn install(inputs: IsolationInputs) {
    if INSTALLED.set(inputs).is_ok() {
        let d = describe(&inputs);
        tracing::info!(
            vmm = d.vmm.as_str(),
            jailer = d.jailer,
            api_tls = d.api_tls,
            summary = d.summary.as_str(),
            "node isolation"
        );
    }
}

/// The installed inputs. With nothing installed it claims no more than the
/// config can prove: the real guest mode, jailer off, TLS off (and logs a warning).
pub fn current() -> IsolationInputs {
    match INSTALLED.get() {
        Some(i) => *i,
        None => {
            tracing::warn!("node isolation was never installed; reporting jailer off, TLS off");
            IsolationInputs {
                guest_mode: crate::guest_mode(),
                use_jailer: false,
                api_tls: false,
                rate_limits: crate::api::rate_limit::current(),
            }
        }
    }
}

/// The isolation line and its parts, from the inputs.
pub fn describe(i: &IsolationInputs) -> NodeIsolation {
    let vm = i.guest_mode == "vm";
    let jailer = vm && i.use_jailer;
    let summary = if !vm {
        "process guest (no isolation)"
    } else if jailer {
        "firecracker microVM, jailer on"
    } else {
        "firecracker microVM, jailer off"
    };
    NodeIsolation {
        vmm: if vm { "firecracker" } else { "process" }.to_string(),
        jailer,
        api_tls: i.api_tls,
        summary: summary.to_string(),
        rate_limits: i.rate_limits.describe(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(mode: &'static str, jailer: bool, tls: bool) -> NodeIsolation {
        describe(&IsolationInputs {
            guest_mode: mode,
            use_jailer: jailer,
            api_tls: tls,
            rate_limits: Default::default(),
        })
    }

    #[test]
    fn microvm_with_jailer() {
        let n = d("vm", true, true);
        assert_eq!(n.summary, "firecracker microVM, jailer on");
        assert_eq!(
            (n.vmm.as_str(), n.jailer, n.api_tls),
            ("firecracker", true, true)
        );
    }

    #[test]
    fn microvm_without_jailer() {
        let n = d("vm", false, false);
        assert_eq!(n.summary, "firecracker microVM, jailer off");
        assert_eq!(
            (n.vmm.as_str(), n.jailer, n.api_tls),
            ("firecracker", false, false)
        );
    }

    #[test]
    fn process_guest_has_no_isolation_whatever_the_jailer_flag() {
        let n = d("process", true, false);
        assert_eq!(n.summary, "process guest (no isolation)");
        assert_eq!((n.vmm.as_str(), n.jailer), ("process", false));
    }

    #[test]
    fn rate_limits_are_reported_and_leave_the_summary_alone() {
        let mut i = IsolationInputs {
            guest_mode: "process",
            use_jailer: false,
            api_tls: false,
            rate_limits: Default::default(),
        };
        let on = describe(&i);
        assert_eq!(on.rate_limits, "on (30/s burst 100, runs 2/s burst 20)");
        i.rate_limits.enabled = false;
        let off = describe(&i);
        assert_eq!(off.rate_limits, "off");
        assert_eq!(on.summary, off.summary);
    }

    #[test]
    fn uninstalled_fallback_claims_nothing_extra() {
        // Nothing installs in the lib test binary: the fallback must not claim the jailer or TLS.
        let c = current();
        assert!(!c.use_jailer && !c.api_tls);
    }
}
