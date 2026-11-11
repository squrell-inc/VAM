use crate::otlp::{install_otlp, shutdown_otlp, OtlpConfig};
use std::sync::atomic::{AtomicBool, Ordering};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry};

static INITIALIZED: AtomicBool = AtomicBool::new(false);

fn init_err(e: impl std::fmt::Display) -> String {
    format!("Failed to set global tracing subscriber: {}", e)
}

/// Initializes the host-wide tracing subscriber for `vamd` or tests (fmt only).
/// Idempotent: returns `Ok(())` if already initialized.
pub fn init_telemetry(service_name: &str) -> Result<(), String> {
    init_telemetry_with_otlp(service_name, &OtlpConfig::disabled())
}

/// Initializes fmt tracing and, when `config.enabled`, the host OTLP export edge.
/// Collector down is fail-open: fmt still works and HTTP/runs are not failed.
pub fn init_telemetry_with_otlp(service_name: &str, config: &OtlpConfig) -> Result<(), String> {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("info,{}=debug", service_name)));

    let json_format = std::env::var("VAM_LOG_FORMAT")
        .map(|v| v == "json")
        .unwrap_or(false);

    let mut otlp_err: Option<String> = None;
    let otel_tracer = if config.enabled {
        match install_otlp(service_name, config) {
            Ok(tracer) => Some(tracer),
            Err(e) => {
                otlp_err = Some(e);
                None
            }
        }
    } else {
        None
    };

    if json_format {
        let fmt_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_target(true)
            .with_thread_ids(true);
        match otel_tracer {
            Some(tracer) => {
                Registry::default()
                    .with(filter)
                    .with(fmt_layer)
                    .with(tracing_opentelemetry::layer().with_tracer(tracer))
                    .try_init()
                    .map_err(init_err)?;
            }
            None => {
                Registry::default()
                    .with(filter)
                    .with(fmt_layer)
                    .try_init()
                    .map_err(init_err)?;
            }
        }
    } else {
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_level(true);
        match otel_tracer {
            Some(tracer) => {
                Registry::default()
                    .with(filter)
                    .with(fmt_layer)
                    .with(tracing_opentelemetry::layer().with_tracer(tracer))
                    .try_init()
                    .map_err(init_err)?;
            }
            None => {
                Registry::default()
                    .with(filter)
                    .with(fmt_layer)
                    .try_init()
                    .map_err(init_err)?;
            }
        }
    }

    let otlp_ok = config.enabled && otlp_err.is_none();
    if let Some(reason) = config.disabled_reason.as_deref() {
        tracing::warn!(
            otlp_disabled_reason = reason,
            endpoint = %config.endpoint,
            "OTLP export disabled"
        );
    }
    if let Some(e) = otlp_err {
        tracing::warn!(
            error = %e,
            endpoint = %config.endpoint,
            "OTLP exporter init failed; continuing without export (fail-open)"
        );
    }
    tracing::info!(
        otlp_enabled = otlp_ok,
        otlp_endpoint = %config.endpoint,
        otlp_protocol = "http",
        success_sample_rate = config.policy.success_sample_rate,
        slow_ms = config.policy.slow_trace_threshold_ms,
        "telemetry subscriber initialized"
    );

    Ok(())
}

/// Flush OTLP providers. Safe to call when OTLP was never enabled.
pub fn shutdown_telemetry() {
    shutdown_otlp();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_telemetry_idempotent() {
        assert!(init_telemetry("vamd-test").is_ok());
        assert!(init_telemetry("vamd-test").is_ok());
    }
}
