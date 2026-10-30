//! `tr-gateway`: see `server/README.md`.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().skip(1).any(|a| a == "-h" || a == "--help") {
        print!("{}", tr_gateway::config::HELP);
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    // reqwest and tungstenite both use rustls; pin one crypto provider, like the desktop app.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cfg = tr_gateway::Config::from_env()?;
    let gw = tr_gateway::start(cfg).await?;
    // The port only: logs may end up on screen, so they never carry addresses.
    tracing::info!("listening on port {} (websocket at /ws)", gw.addr.port());
    // Ctrl-C, or SIGTERM from a service manager.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    gw.shutdown().await;
    Ok(())
}
