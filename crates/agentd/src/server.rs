//! Both listeners share the same router, authentication and shutdown lifecycle.
use crate::{hub::Hub, local::Peer};
use std::{net::SocketAddr, sync::Arc};
use tokio_util::sync::CancellationToken;

pub async fn serve(hub: Arc<Hub>, bind: Option<SocketAddr>) -> anyhow::Result<()> {
    hub.config.validate()?;
    anyhow::ensure!(bind.is_some() || hub.config.unix_socket.is_some(), "no listener enabled (use --unix-socket or --bind)");
    if let Some(trust) = &hub.config.trust_loopback {
        tracing::warn!("trustLoopback enabled: any local process can act as client {}", trust.client);
    }
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let tcp = match bind {
        Some(bind) => Some(tokio::net::TcpListener::bind(bind).await?),
        None => None,
    };
    #[cfg(unix)]
    let uds = hub.config.unix_socket.as_deref().map(|path| crate::local::socket::bind(path, hub.config.socket_mode()?)).transpose()?;
    let shutdown = CancellationToken::new();
    let mut servers = tokio::task::JoinSet::new();
    let app = crate::http::app(hub.clone());
    if let Some(listener) = tcp {
        tracing::info!(address = %listener.local_addr()?, "TCP listening");
        let stop = shutdown.clone();
        let app = app.clone();
        servers.spawn(async move { axum::serve(listener, app.into_make_service_with_connect_info::<Peer>()).with_graceful_shutdown(stop.cancelled_owned()).await });
    }
    #[cfg(unix)]
    let _socket_guard = if let Some((listener, guard)) = uds {
        tracing::info!("Unix socket listening");
        let stop = shutdown.clone();
        servers.spawn(async move { axum::serve(listener, app.into_make_service_with_connect_info::<Peer>()).with_graceful_shutdown(stop.cancelled_owned()).await });
        Some(guard)
    } else {
        None
    };
    let mut sweep = tokio::task::JoinSet::new();
    if let Some(period) = hub.sweep_interval() {
        let hub = hub.clone();
        sweep.spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                hub.evict_idle(std::time::Instant::now());
            }
        });
    }
    let result = tokio::select! {
        result = servers.join_next() => result,
        _ = tokio::signal::ctrl_c() => None,
        _ = sigterm.recv() => None,
    };
    tracing::info!("shutting down; aborting sessions");
    shutdown.cancel();
    // abort_all bounds session settlement, reporter flush and store flush by shutdownGraceSecs.
    hub.abort_all().await;
    // Idle SSE subscribers must not hold shutdown (and socket cleanup) forever.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(result) = servers.join_next().await {
            result??;
        }
        anyhow::Ok(())
    })
    .await;
    if let Some(result) = result {
        result??;
    }
    Ok(())
}
