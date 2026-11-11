use anyhow::Result;

use crate::client::{self, Client};
use crate::config;

pub async fn run(host: Option<String>, token: Option<String>) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    let body = c.get("/v1/health").await?;
    client::print_json(&body);
    Ok(())
}
