//! Binary entry point: load `.env`, read config, serve with a bounded graceful
//! shutdown.

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("agent_runtime=info,tower_http=info")),
        )
        .init();

    let config = agent_runtime::Config::from_env()?;
    let addr = config.bind;
    let grace = config.shutdown_grace;
    let (app, state) = agent_runtime::build(config);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("agent-runtime listening on http://{addr}");

    // On SIGINT/SIGTERM: stop accepting, let in-flight turns finish, then
    // close kept sessions and warm spares so no `claude` process outlives the
    // server. Each phase is bounded by the grace period; whatever is still
    // running when it elapses is dropped, and the SDK kills its process.
    let (signal_tx, signal_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = signal_tx.send(true);
    });
    let graceful = {
        let mut rx = signal_rx.clone();
        async move {
            let _ = rx.wait_for(|signalled| *signalled).await;
        }
    };
    let grace_elapsed = {
        let mut rx = signal_rx;
        async move {
            let _ = rx.wait_for(|signalled| *signalled).await;
            tokio::time::sleep(grace).await;
        }
    };
    tokio::select! {
        served = axum::serve(listener, app).with_graceful_shutdown(graceful) => served?,
        _ = grace_elapsed => {
            tracing::warn!(grace_secs = grace.as_secs(), "shutdown grace period elapsed with requests still in flight");
        }
    }

    tracing::info!(
        sessions = state.pool.len(),
        warm = state.warm.ready(),
        "closing agent processes"
    );
    if tokio::time::timeout(grace, state.shutdown()).await.is_err() {
        tracing::warn!("some agent processes did not exit in time; they are killed on exit");
    }
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
