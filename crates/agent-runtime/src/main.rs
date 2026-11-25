//! Binary entry point: load `.env`, read config, serve.

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("agent_runtime=info")),
        )
        .init();

    let config = agent_runtime::Config::from_env()?;
    let addr = config.bind;
    let app = agent_runtime::app(config);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("agent-runtime listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
