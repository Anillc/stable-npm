mod db;
mod handler;
mod service;
mod timeutil;
mod upstream;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

/// Read-only, time-traveling proxy for the npm registry.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Address to listen on
    #[arg(long, default_value = "127.0.0.1:3000")]
    addr: String,

    /// Path to the SQLite database
    #[arg(long, default_value = "stable-npm.db")]
    db: String,

    /// Upstream registry
    #[arg(long, default_value = "https://registry.npmjs.org")]
    upstream: String,

    /// Upstream request timeout, in seconds
    #[arg(long, default_value_t = 120)]
    timeout: u64,

    /// In-memory response cache, in MiB
    #[arg(long, default_value_t = 128)]
    cache_mb: u64,

    /// Log filter, in tracing `EnvFilter` syntax
    #[arg(long, default_value = "info")]
    log: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log).context("invalid --log filter")?)
        .init();

    tracing::info!(
        addr = %cli.addr,
        upstream = %cli.upstream,
        db = %cli.db,
        "starting stable-npm"
    );

    let db = db::Db::open(&cli.db).await.context("open database")?;
    let upstream = upstream::Upstream::new(&cli.upstream, Duration::from_secs(cli.timeout));
    let service = Arc::new(service::Service::new(
        db,
        upstream,
        cli.cache_mb.saturating_mul(1024 * 1024),
    ));

    let listener = TcpListener::bind(&cli.addr)
        .await
        .with_context(|| format!("bind {}", cli.addr))?;
    tracing::info!("listening on http://{}", cli.addr);

    axum::serve(listener, handler::router(service))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve")?;
    Ok(())
}

async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to listen for ctrl-c");
        }
    };

    // As PID 1 in a container, SIGTERM is ignored unless we handle it.
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::error!(%error, "failed to listen for SIGTERM"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
}
