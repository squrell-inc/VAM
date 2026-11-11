//! Standalone echo broker for manual two-process HSP transport proof.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
#[cfg(unix)]
use vam_orchestrator::hsp_echo::{bind_unix_listener, serve_unix};
use vam_orchestrator::hsp_echo::{
    fire_grant, serve_tcp, serve_unix_triggers, EchoBroker, FireParams, TriggerTestBroker,
    GRANT_TOKEN_FILE,
};

#[derive(Debug, Parser)]
#[command(
    name = "hsp-echo-broker",
    about = "Phase 1 HSP echo broker over UDS (and optional TCP)"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Unix domain socket path (required for UDS transport).
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Optional TCP loopback address for test/debug (e.g. 127.0.0.1:0).
    #[arg(long)]
    tcp: Option<SocketAddr>,

    /// Serve the trigger test broker (tool `echo_arm_trigger`) instead of the echo broker.
    #[arg(long)]
    triggers: bool,

    /// Serve the effects test broker (tools `echo_create_ticket`, `echo_create_note`,
    /// `echo_find_ticket`; every call is written to `<state-dir>/ledger.jsonl`).
    #[arg(long)]
    effects: bool,

    /// Where the grant token is kept (default: the socket's directory).
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// With --triggers: the gateway socket, used by an armed `after_ms` firing.
    #[arg(long)]
    gateway_socket: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Print the effects ledger: one JSON line per (tool, title) with its count.
    Ledger {
        #[arg(long)]
        state_dir: PathBuf,
    },
    /// Fire the kept grant against the gateway; prints one JSON line per fire.
    Fire {
        #[arg(long)]
        state_dir: PathBuf,
        /// The gateway socket vamd reports for this broker (required).
        #[arg(long)]
        gateway_socket: PathBuf,
        #[arg(long)]
        key: String,
        #[arg(long, default_value_t = 1)]
        times: u32,
        /// scheduled | run-now
        #[arg(long, default_value = "scheduled")]
        kind: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();

    if let Some(Cmd::Ledger { state_dir }) = &args.command {
        for l in vam_orchestrator::hsp_echo_effects::ledger_counts(state_dir) {
            println!("{l}");
        }
        return Ok(());
    }

    if let Some(Cmd::Fire {
        state_dir,
        gateway_socket,
        key,
        times,
        kind,
    }) = args.command
    {
        let kind = match kind.as_str() {
            "scheduled" => vam_spec::v1::FireKind::Scheduled,
            "run-now" | "run_now" => vam_spec::v1::FireKind::RunNow,
            other => return Err(format!("unknown --kind {other}").into()),
        };
        let token = std::fs::read_to_string(state_dir.join(GRANT_TOKEN_FILE))
            .map_err(|e| format!("{}/{GRANT_TOKEN_FILE}: {e}", state_dir.display()))?;
        let lines = fire_grant(
            &gateway_socket,
            token.trim(),
            &FireParams { key, times, kind },
        )
        .await?;
        for l in lines {
            println!("{l}");
        }
        return Ok(());
    }

    #[cfg(unix)]
    {
        let socket = args.socket.clone().unwrap_or_default();
        let listener = bind_unix_listener(&socket)?;
        if args.effects {
            let dir = args
                .state_dir
                .clone()
                .or_else(|| socket.parent().map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("."));
            vam_orchestrator::hsp_echo_effects::serve_unix_effects(
                listener,
                vam_orchestrator::hsp_echo_effects::EffectsTestBroker::new(dir),
            )
            .await?;
            return Ok(());
        }
        if args.triggers {
            let dir = args
                .state_dir
                .clone()
                .or_else(|| socket.parent().map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("."));
            serve_unix_triggers(listener, TriggerTestBroker::new(dir, args.gateway_socket)).await?;
            return Ok(());
        }
        if let Some(tcp_addr) = args.tcp {
            tokio::spawn(async move {
                if let Err(err) = serve_tcp(tcp_addr, EchoBroker).await {
                    eprintln!("hsp echo tcp server exited: {err}");
                }
            });
            serve_unix(listener, EchoBroker).await?;
        } else {
            serve_unix(listener, EchoBroker).await?;
        }
    }

    #[cfg(not(unix))]
    {
        if let Some(tcp_addr) = args.tcp {
            serve_tcp(tcp_addr, EchoBroker).await?;
        } else {
            eprintln!("UDS is not supported on non-Unix platforms. Please specify --tcp <addr>.");
        }
    }

    Ok(())
}
