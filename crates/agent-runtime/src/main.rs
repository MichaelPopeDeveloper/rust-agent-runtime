//! Binary entry point: load `.env`, read config, serve with graceful shutdown.

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
    let (app, state) = agent_runtime::build(config);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("agent-runtime listening on http://{addr}");
    // Stop accepting on SIGINT/SIGTERM, let in-flight turns finish (bounded by
    // the run timeout), then disconnect kept sessions so no `claude` process
    // is left orphaned by the exit.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!(live = state.pool.len(), "shutting down agent sessions");
    state.shutdown().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
