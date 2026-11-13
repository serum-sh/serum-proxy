//! serum proxy entry point.

// Set here, not in Cargo.toml `[lints]`, so tests can still unwrap.
#![warn(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tokio::sync::{RwLock, watch};
use tracing::{error, info, warn};

use serum_proxy::{admin, config, egress, executor, logging, session};

#[derive(Parser, Debug)]
#[command(version, about = "serum outbound incident-response proxy")]
struct Cli {
    #[arg(long, default_value = "/app/config.yaml")]
    config: PathBuf,
    #[arg(long, env = "SERUM_PROXY_TOKEN")]
    token: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!(" serum proxy v{}", env!("CARGO_PKG_VERSION"));

    let cli = Cli::parse();
    let cfg = config::Config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config.display()))?;
    logging::init(&cfg.proxy.log_level);

    info!(
        services = cfg.services.len(),
        admin_bind = %cfg.proxy.admin_bind,
        "config loaded"
    );

    // Resolve each service (guardrails + TLS client + token source) and log its allowlists.
    let mut registry: session::ServiceRegistry = HashMap::new();
    let mut service_names = Vec::new();
    for svc in &cfg.services {
        let resolved = executor::ResolvedService::from_config(svc)?;
        info!(
            service = %svc.kind,
            allowlist = ?resolved.guardrails.allowlist,
            path_allowlist = ?svc.path_allowlist,
            max_concurrent = resolved.guardrails.max_permits,
            queue_ceiling = resolved.guardrails.queue_ceiling,
            private_ca = resolved.tls_client.is_some(),
            auth_token_file = resolved.token_source.is_some(),
            "service ready"
        );
        for path in &resolved.guardrails.unused_path_allowlist {
            warn!(
                service = %svc.kind,
                path = %path,
                "path_allowlist entry overlaps no built-in entry; it allows nothing"
            );
        }
        service_names.push(svc.kind.to_string());
        registry.insert(svc.kind.to_string(), Arc::new(resolved));
    }
    let registry = Arc::new(registry);

    let egress_buf = egress::EgressBuffer::new(egress::DEFAULT_CAPACITY);
    let exec = Arc::new(executor::Executor::new(
        executor::build_client(),
        egress_buf.clone(),
    ));

    let admin_bind: std::net::SocketAddr = cfg
        .proxy
        .admin_bind
        .parse()
        .with_context(|| format!("invalid admin_bind: {}", cfg.proxy.admin_bind))?;
    let session_status = Arc::new(RwLock::new(admin::SessionStatus::default()));
    let admin_state = admin::AdminState {
        session: session_status.clone(),
        egress: egress_buf.clone(),
        services: service_names,
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Spawn admin server.
    let admin_handle = tokio::spawn({
        let admin_state = admin_state.clone();
        async move {
            if let Err(e) = admin::serve(admin_bind, admin_state).await {
                error!(error = %e, "admin server crashed");
            }
        }
    });

    // Spawn session.
    let session_cfg = session::SessionConfig {
        url: cfg.proxy.serum_wss_url.clone(),
        token: cli.token.clone(),
        heartbeat_interval: session::HEARTBEAT_INTERVAL,
        max_backoff: Duration::from_secs(cfg.proxy.reconnect_max_backoff_secs),
        proxy_build_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let session_handle = tokio::spawn({
        let registry = registry.clone();
        let exec = exec.clone();
        let status = session_status.clone();
        let shutdown_rx = shutdown_rx.clone();
        async move {
            if let Err(e) = session::run(session_cfg, registry, exec, status, shutdown_rx).await {
                error!(error = %e, "session task exited with error");
            }
        }
    });

    wait_for_shutdown().await;
    info!("shutdown signal received; draining in-flight requests");
    let _ = shutdown_tx.send(true);

    // Bounded grace period.
    let drain = tokio::time::timeout(Duration::from_secs(10), session_handle).await;
    if drain.is_err() {
        error!("session task did not drain within grace period; forcing exit");
    }
    admin_handle.abort();
    info!("shutdown complete");
    Ok(())
}

/// Block until SIGINT or SIGTERM (on Unix).
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
