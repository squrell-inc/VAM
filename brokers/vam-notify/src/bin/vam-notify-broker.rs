//! `vam-notify-broker`: the standalone HSP notification channel broker process.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{CommandFactory, Parser, Subcommand};
use vam_notify::adapter::Registry;
use vam_notify::gateway::GatewayClient;
use vam_notify::hub::Hub;
use vam_notify::log::DeliveryLog;
use vam_notify::manifest::MANIFEST_JSON;
use vam_notify::serve::{bind_unix_listener, serve_unix};
use vam_notify::slack::SlackAdapter;
use vam_notify::state::StateLock;
use vam_notify::transport::{LiveTransport, SlackTransport};
use vam_notify::NotifyBroker;

#[derive(Debug, Parser)]
#[command(
    name = "vam-notify-broker",
    about = "HSP notification channel broker: delivers vamd's neutral renderings to channel adapters (Slack).",
    args_conflicts_with_subcommands = true
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Unix domain socket vamd dials.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Directory for the broker's delivery log and lock (0700).
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// The notification gateway socket vamd reports after the broker is registered.
    #[arg(long)]
    gateway_socket: Option<PathBuf>,

    /// Test builds only: use the fake Slack transport.
    #[cfg(feature = "fake-transport")]
    #[arg(long)]
    fake_transport: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the manifest to register with `PUT /v1/hosted/brokers`.
    Manifest,
    /// Test builds only: drive the fake transport.
    #[cfg(feature = "fake-transport")]
    Fake {
        #[command(subcommand)]
        action: FakeAction,
    },
}

#[cfg(feature = "fake-transport")]
#[derive(Debug, Subcommand)]
enum FakeAction {
    /// A Slack user presses a button of the newest message of a kind.
    Click {
        #[arg(long)]
        state_dir: PathBuf,
        /// The clicking Slack account, `<team>/<user>`.
        #[arg(long)]
        account: String,
        /// A message kind (`approval.pending`) or `latest`.
        #[arg(long, default_value = "latest")]
        message: String,
        /// The id of the action pressed (`approve`, `deny`, `opt_0`, ...).
        #[arg(long)]
        action: String,
        /// `forged`, `replay` or `from:<account>`; omitted, the message's own handle.
        #[arg(long)]
        handle: Option<String>,
        /// Send the call through this gateway socket instead of the broker's own.
        #[arg(long)]
        gateway_socket: Option<PathBuf>,
    },
    /// A Slack user writes to the app.
    Dm {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        account: String,
        #[arg(long)]
        text: String,
    },
    /// Call ActOnNotification with any handle and decision.
    ActRaw {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        account: String,
        #[arg(long)]
        handle: String,
        #[arg(long)]
        decision: String,
        #[arg(long)]
        gateway_socket: Option<PathBuf>,
    },
    /// List the delivery ids the broker holds a message for (never the handles).
    Handles {
        #[arg(long)]
        state_dir: PathBuf,
    },
}

#[cfg(feature = "fake-transport")]
async fn run_fake(action: FakeAction) -> Result<(), String> {
    use vam_notify::control::{run_cli, Request};
    match action {
        FakeAction::Click {
            state_dir,
            account,
            message,
            action,
            handle,
            gateway_socket,
        } => {
            run_cli(
                &state_dir,
                Request::Click {
                    account,
                    message,
                    action,
                    handle,
                    gateway_socket,
                },
            )
            .await
        }
        FakeAction::Dm {
            state_dir,
            account,
            text,
        } => run_cli(&state_dir, Request::Dm { account, text }).await,
        FakeAction::ActRaw {
            state_dir,
            account,
            handle,
            decision,
            gateway_socket,
        } => {
            run_cli(
                &state_dir,
                Request::ActRaw {
                    account,
                    handle,
                    decision,
                    gateway_socket,
                },
            )
            .await
        }
        FakeAction::Handles { state_dir } => run_cli(&state_dir, Request::Handles).await,
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("vam-notify-broker: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    match args.command {
        Some(Command::Manifest) => {
            println!("{MANIFEST_JSON}");
            return Ok(());
        }
        #[cfg(feature = "fake-transport")]
        Some(Command::Fake { action }) => {
            return run_fake(action).await.map_err(Into::into);
        }
        None => {}
    }
    let (Some(socket), Some(state_dir)) = (args.socket.clone(), args.state_dir.clone()) else {
        Args::command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "--socket and --state-dir are required",
            )
            .exit();
    };

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let state = StateLock::open(&state_dir)?;
    let gateway = GatewayClient::new(args.gateway_socket.clone());
    #[allow(unused_mut)]
    let mut transport: Arc<dyn SlackTransport> = Arc::new(LiveTransport::new());
    #[cfg(feature = "fake-transport")]
    if args.fake_transport {
        tracing::warn!(
            "{}: no Slack call leaves this process",
            vam_notify::transport::fake::MARKER
        );
        transport = Arc::new(vam_notify::transport::fake::FakeTransport::new(state.dir()));
    }
    let registry = Registry::default().with(Arc::new(SlackAdapter::new(transport)));
    let log = DeliveryLog::open(&state.db_path())?;
    let hub = Hub::new(registry, log, gateway.clone());
    #[cfg(feature = "fake-transport")]
    if args.fake_transport {
        let control = vam_notify::control::bind(state.dir())?;
        let hub = hub.clone();
        tokio::spawn(async move {
            if let Err(e) = vam_notify::control::serve(hub, control).await {
                tracing::error!(error = %e, "control socket stopped");
            }
        });
    }
    // The app-level token is fetched with GetChannelConfig on start and on every reconnect, used
    // for the connect call and dropped.
    tokio::spawn(hub.clone().run_connection(vam_notify::slack::CHANNEL));
    let broker = NotifyBroker::new(hub);
    let listener = bind_unix_listener(&socket)?;
    tracing::info!(
        socket = %socket.display(),
        state_dir = %state.dir().display(),
        gateway = gateway.is_configured(),
        "vam-notify-broker listening"
    );
    serve_unix(listener, broker).await?;
    Ok(())
}
