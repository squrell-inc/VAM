//! `vam-schedule-broker`: the standalone HSP schedule broker process.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{CommandFactory, Parser, Subcommand};
use vam_schedule::config::parse_interval;
use vam_schedule::fire::Scheduler;
use vam_schedule::gateway::GatewayClient;
use vam_schedule::manifest::MANIFEST_JSON;
use vam_schedule::serve::{bind_unix_listener, serve_unix};
use vam_schedule::state::StateLock;
use vam_schedule::store::Store;
use vam_schedule::{ScheduleBroker, SystemClock};

#[derive(Debug, Parser)]
#[command(
    name = "vam-schedule-broker",
    about = "HSP schedule broker: schedule_create, schedule_list, schedule_get, schedule_update, schedule_delete, schedule_run_now.",
    args_conflicts_with_subcommands = true
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Unix domain socket vamd dials.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Directory for the broker's database and lock (0700).
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// The trigger gateway socket vamd reports after the broker is registered.
    #[arg(long)]
    gateway_socket: Option<PathBuf>,

    /// The least time between two fires of a cron schedule (for example 15m).
    #[arg(long, default_value = "15m")]
    min_interval: String,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the manifest to register with `PUT /v1/hosted/brokers`.
    Manifest,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("vam-schedule-broker: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    if let Some(Command::Manifest) = args.command {
        println!("{MANIFEST_JSON}");
        return Ok(());
    }
    let (Some(socket), Some(state_dir)) = (args.socket.clone(), args.state_dir.clone()) else {
        Args::command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "--socket and --state-dir are required",
            )
            .exit();
    };
    let min_interval = parse_interval(&args.min_interval)?;

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Lock first: a second process on this store exits before it touches the socket or the db.
    let state = StateLock::open(&state_dir)?;
    let store = Arc::new(Store::open(&state.db_path())?);
    let gateway = GatewayClient::new(args.gateway_socket.clone());
    if !gateway.is_configured() {
        tracing::info!("no --gateway-socket: fires wait until the broker is started with one");
    }
    // Fires run only when the gateway is known; without it the rows are left untouched (their
    // times are caught up when the broker is next started with the flag).
    let (stop_loop, loop_shutdown) = tokio::sync::watch::channel(false);
    let mut broker =
        ScheduleBroker::new(store.clone(), gateway.clone()).with_min_interval(min_interval);
    let fire_loop = gateway.is_configured().then(|| {
        let sched = Scheduler::new(
            store.clone(),
            Arc::new(gateway.clone()),
            Arc::new(SystemClock),
        );
        // A create or update wakes the loop so it re-reads the earliest time.
        broker = broker.clone().with_wake(sched.wake_handle());
        tokio::spawn(sched.run(loop_shutdown))
    });
    let listener = bind_unix_listener(&socket)?;
    tracing::info!(
        socket = %socket.display(),
        state_dir = %state.dir().display(),
        gateway = args.gateway_socket.is_some(),
        min_interval_secs = min_interval.as_secs(),
        "vam-schedule-broker listening"
    );
    let served = serve_unix(listener, broker).await;
    let _ = stop_loop.send(true);
    if let Some(h) = fire_loop {
        let _ = h.await;
    }
    served?;
    Ok(())
}
