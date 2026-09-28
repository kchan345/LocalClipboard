//! LocalClipboard: share text, files and folders between devices on a local
//! network from a single executable. Nothing is stored on the server.

pub mod config;
pub mod http;
pub mod hub;
pub mod net_util;
pub mod relay;
pub mod timefmt;
pub mod zip;

use std::net::SocketAddr;

pub use config::Config;
pub use hub::{AppState, Shared};

/// Serves the application on `listener` until the process exits.
pub async fn serve(listener: tokio::net::TcpListener, state: Shared) -> std::io::Result<()> {
    axum::serve(
        listener,
        http::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}
