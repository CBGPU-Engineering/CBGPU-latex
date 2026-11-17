//! Stop accepting HTTP on SIGTERM and drain accepted requests without a forced timeout.
//! Upgraded editor WebSockets must be absent before an operational handoff; this
//! helper does not pretend that Axum owns detached WebSocket application tasks.
pub async fn signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _=terminate.recv()=>{}, _=tokio::signal::ctrl_c()=>{} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
pub async fn serve(listener: tokio::net::TcpListener, router: axum::Router) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(signal())
        .await
}
