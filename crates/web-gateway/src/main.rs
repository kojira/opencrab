//! web-gateway 独立 binary。HTTP/SSE ⇄ V3 UDS 変換のみ。Bearer は持たない。

use std::path::PathBuf;

use anyhow::Context;
use opencrab_web_gateway::v3::client::InstanceClient;
use opencrab_web_gateway::v3::config::Placement;
use opencrab_web_gateway::v3::http::{router, HttpState};
use opencrab_web_gateway::v3::wire::config_digest;

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("runtime")?;
    rt.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("opencrab_web_gateway=info".parse()?)
                .add_directive("opencrab_gate_client=info".parse()?)
                .add_directive("web_gateway=info".parse()?),
        )
        .init();

    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .context("usage: web-gateway <placement.json>")?;
    let place = Placement::load(&path)?;
    let socket = PathBuf::from(&place.core_socket);

    let mut instances = Vec::new();
    for inst in &place.instances {
        let digest = config_digest(&inst.author_id);
        instances.push(InstanceClient::spawn(
            socket.clone(),
            inst.instance_id.clone(),
            inst.revision,
            inst.author_id.clone(),
            digest,
        ));
    }

    let bind: std::net::SocketAddr = place.http_bind.parse()?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {}", place.http_bind))?;
    tracing::info!(addr = %place.http_bind, "web-gateway listening");
    let app = router(HttpState { instances });
    axum::serve(listener, app).await.context("serve")?;
    Ok(())
}
