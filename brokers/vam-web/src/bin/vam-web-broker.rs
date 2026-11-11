//! `vam-web-broker`: standalone HSP web broker process.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, ValueEnum};
use vam_web::fetch::{FixtureFetcher, NetworkFetcher, PageFetcher, DEFAULT_DOCS_URL};
use vam_web::provider::brave::BraveProvider;
use vam_web::provider::mock::MockProvider;
use vam_web::provider::SearchProvider;
use vam_web::serve::serve_tcp;
#[cfg(unix)]
use vam_web::serve::{bind_unix_listener, serve_unix};
use vam_web::WebBroker;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderKind {
    Brave,
    Mock,
}

#[derive(Debug, Parser)]
#[command(
    name = "vam-web-broker",
    about = "HSP web broker: web_search and web_fetch. The search key arrives per invoke from vamd, never from the environment."
)]
struct Args {
    /// Unix domain socket path.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// TCP address for tests and debugging (for example 127.0.0.1:0).
    #[arg(long)]
    tcp: Option<SocketAddr>,

    /// Search provider.
    #[arg(long, value_enum, default_value = "brave")]
    provider: ProviderKind,

    /// Directory of fixtures (required with --provider mock): search JSON files
    /// and `pages/` for web_fetch. With the mock provider web_fetch never
    /// touches the network.
    #[arg(long)]
    mock_fixtures: Option<PathBuf>,

    /// Page that describes this fetcher; sent in the User-Agent
    /// (`VAM-WebFetch/1 (+<url>)`) so site owners can find out who fetches.
    #[arg(long, default_value = DEFAULT_DOCS_URL)]
    docs_url: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();

    let (provider, fetcher): (Arc<dyn SearchProvider>, Arc<dyn PageFetcher>) = match args.provider {
        ProviderKind::Brave => (
            Arc::new(BraveProvider::new()?),
            Arc::new(NetworkFetcher::new(&args.docs_url)),
        ),
        ProviderKind::Mock => {
            let dir = args
                .mock_fixtures
                .ok_or("--provider mock requires --mock-fixtures <dir>")?;
            (
                Arc::new(MockProvider::new(dir.clone())),
                Arc::new(FixtureFetcher::new(dir)),
            )
        }
    };
    let broker = WebBroker::new(provider, fetcher);

    if args.socket.is_none() && args.tcp.is_none() {
        return Err("give --socket <path> and/or --tcp <addr>".into());
    }

    let mut tasks = Vec::new();
    if let Some(addr) = args.tcp {
        let broker = broker.clone();
        tasks.push(tokio::spawn(async move {
            serve_tcp(addr, broker).await.map_err(|e| e.to_string())
        }));
    }
    #[cfg(unix)]
    if let Some(path) = args.socket {
        let listener = bind_unix_listener(&path)?;
        tracing::info!(socket = %path.display(), "vam-web-broker listening");
        let broker = broker.clone();
        tasks.push(tokio::spawn(async move {
            serve_unix(listener, broker)
                .await
                .map_err(|e| e.to_string())
        }));
    }
    #[cfg(not(unix))]
    if args.socket.is_some() {
        return Err("UDS is not supported on this platform; use --tcp".into());
    }

    for task in tasks {
        task.await??;
    }
    Ok(())
}
